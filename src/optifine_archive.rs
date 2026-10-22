use crate::{
    metadata::{safe_path, valid_id},
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    fs,
    io::{Cursor, Read, Write},
    path::Path,
};

pub(crate) fn digest(path: &Path, control: &TaskControl) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha1::new();
    let mut buffer = [0; 65536];
    loop {
        control.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub(crate) fn bytes(
    archive: &mut zip::ZipArchive<fs::File>,
    name: &str,
    limit: u64,
    control: &TaskControl,
) -> Result<Vec<u8>> {
    control.check()?;
    let entry = archive.by_name(name)?;
    if entry.size() > limit || entry.is_dir() {
        bail!("OptiFine 条目过大或类型错误：{name}");
    }
    let mut bytes = vec![];
    entry.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("OptiFine 条目超出限制");
    }
    control.check()?;
    Ok(bytes)
}
struct ClassReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> ClassReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(count).context("class 偏移溢出")?;
        let value = self
            .bytes
            .get(self.offset..end)
            .context("OptiFine Config class 不完整")?;
        self.offset = end;
        Ok(value)
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}
enum Constant {
    Other,
    Utf8(Vec<u8>),
    String(u16),
}
fn utf8(pool: &[Constant], index: u16) -> Result<&[u8]> {
    match pool.get(usize::from(index)) {
        Some(Constant::Utf8(bytes)) => Ok(bytes),
        _ => bail!("无效 class UTF-8 索引"),
    }
}
fn config_constants(bytes: &[u8]) -> Result<HashMap<String, String>> {
    let mut reader = ClassReader { bytes, offset: 0 };
    if reader.u32()? != 0xcafebabe {
        bail!("Config 不是 class 文件");
    }
    reader.take(4)?;
    let count = reader.u16()?;
    let mut pool = vec![Constant::Other];
    let mut i = 1;
    while i < count {
        let tag = reader.take(1)?[0];
        let constant = match tag {
            1 => {
                let len = reader.u16()?;
                Constant::Utf8(reader.take(usize::from(len))?.to_vec())
            }
            8 => Constant::String(reader.u16()?),
            3 | 4 => {
                reader.take(4)?;
                Constant::Other
            }
            5 | 6 => {
                reader.take(8)?;
                pool.push(Constant::Other);
                i += 1;
                Constant::Other
            }
            7 | 16 | 19 | 20 => {
                reader.take(2)?;
                Constant::Other
            }
            9..=12 | 17 | 18 => {
                reader.take(4)?;
                Constant::Other
            }
            15 => {
                reader.take(3)?;
                Constant::Other
            }
            _ => bail!("未知 class 常量类型：{tag}"),
        };
        pool.push(constant);
        i += 1;
    }
    reader.take(6)?;
    let interfaces = reader.u16()?;
    reader.take(usize::from(interfaces) * 2)?;
    let fields = reader.u16()?;
    let mut result = HashMap::new();
    for _ in 0..fields {
        reader.take(2)?;
        let name = reader.u16()?;
        reader.take(2)?;
        let attributes = reader.u16()?;
        let name = utf8(&pool, name)?;
        let wanted = matches!(name, b"MC_VERSION" | b"OF_EDITION" | b"OF_RELEASE");
        for _ in 0..attributes {
            let attribute = reader.u16()?;
            let length = reader.u32()?;
            if wanted && utf8(&pool, attribute)? == b"ConstantValue" {
                if length != 2 {
                    bail!("OptiFine 常量属性长度错误");
                }
                let index = reader.u16()?;
                let value = match pool.get(usize::from(index)) {
                    Some(Constant::String(index)) => utf8(&pool, *index)?,
                    _ => bail!("OptiFine 版本字段不是字符串常量"),
                };
                let value = std::str::from_utf8(value)?.to_owned();
                let name = std::str::from_utf8(name)?.to_owned();
                if result.insert(name, value).is_some() {
                    bail!("OptiFine 版本字段重复");
                }
            } else {
                reader.take(usize::try_from(length)?)?;
            }
        }
    }
    Ok(result)
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Identity {
    pub game: String,
    pub edition: String,
    pub release: String,
}
impl Identity {
    pub(crate) fn version(&self) -> String {
        format!("{}_{}", self.edition, self.release)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        for part in [&self.game, &self.edition, &self.release] {
            valid_id(part)?;
            if part.len() > 64
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            {
                bail!("OptiFine 版本字段无效");
            }
        }
        valid_id(&format!("{}_{}", self.game, self.version()))
    }
}
pub(crate) struct Wrapper {
    pub artifact: &'static str,
    pub version: String,
    pub entry: String,
}
pub(crate) struct OptiFineArchive {
    pub identity: Identity,
    pub patcher: bool,
    pub wrapper: Option<Wrapper>,
    pub build: Option<String>,
    pub tweaker: bool,
    pub forge_tweaker: bool,
}
impl OptiFineArchive {
    pub(crate) fn inspect(path: &Path, control: &TaskControl) -> Result<Self> {
        let mut archive = crate::installer_archive::open(path, control)?;
        let config = [
            "Config.class",
            "net/optifine/Config.class",
            "notch/net/optifine/Config.class",
        ]
        .into_iter()
        .find(|name| archive.file_names().any(|entry| entry == *name))
        .context("安装包缺少 OptiFine Config class")?;
        let mut values = config_constants(&bytes(&mut archive, config, 4 * 1024 * 1024, control)?)?;
        let mut field = |name| {
            values
                .remove(name)
                .with_context(|| format!("OptiFine 缺少 {name} 常量"))
        };
        let identity = Identity {
            game: field("MC_VERSION")?,
            edition: field("OF_EDITION")?,
            release: field("OF_RELEASE")?,
        };
        identity.validate()?;
        let patcher = archive
            .file_names()
            .any(|entry| entry == "optifine/Patcher.class");
        let wrapper = if archive
            .file_names()
            .any(|entry| entry == "launchwrapper-of.txt")
        {
            let version =
                String::from_utf8(bytes(&mut archive, "launchwrapper-of.txt", 1024, control)?)?
                    .trim()
                    .to_owned();
            valid_id(&version)?;
            if !version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            {
                bail!("无效 OptiFine LaunchWrapper 版本");
            }
            let entry = format!("launchwrapper-of-{version}.jar");
            zip::ZipArchive::new(Cursor::new(bytes(
                &mut archive,
                &entry,
                16 * 1024 * 1024,
                control,
            )?))
            .context("OptiFine LaunchWrapper JAR 无效")?;
            Some(Wrapper {
                artifact: "launchwrapper-of",
                version,
                entry,
            })
        } else if archive
            .file_names()
            .any(|entry| entry == "launchwrapper-2.0.jar")
        {
            zip::ZipArchive::new(Cursor::new(bytes(
                &mut archive,
                "launchwrapper-2.0.jar",
                16 * 1024 * 1024,
                control,
            )?))?;
            Some(Wrapper {
                artifact: "launchwrapper",
                version: "2.0".into(),
                entry: "launchwrapper-2.0.jar".into(),
            })
        } else {
            None
        };
        let build = if archive.file_names().any(|entry| entry == "buildof.txt") {
            Some(
                String::from_utf8(bytes(&mut archive, "buildof.txt", 1024, control)?)?
                    .trim()
                    .to_owned(),
            )
        } else {
            None
        };
        let tweaker = archive
            .file_names()
            .any(|entry| entry == "optifine/OptiFineTweaker.class");
        let forge_tweaker = archive
            .file_names()
            .any(|entry| entry == "optifine/OptiFineForgeTweaker.class");
        Ok(Self {
            identity,
            patcher,
            wrapper,
            build,
            tweaker,
            forge_tweaker,
        })
    }
}
/// Store entries in a fixed order with fixed timestamps and without compression,
/// so re-running the patcher can reproduce the exact published file checksum.
pub(crate) fn normalize(input: &Path, output: &Path, control: &TaskControl) -> Result<()> {
    let mut archive = crate::installer_archive::open(input, control)?;
    let mut names: Vec<_> = archive.file_names().map(str::to_owned).collect();
    names.sort();
    let mut writer = zip::ZipWriter::new(fs::File::create(output)?);
    let mut total = 0u64;
    let mut actual_total = 0u64;
    for name in names {
        control.check()?;
        let upper = name.to_ascii_uppercase();
        if name.ends_with('/')
            || upper == "META-INF/MODS.TOML"
            || upper.starts_with("META-INF/")
                && [".SF", ".RSA", ".DSA", ".EC"]
                    .iter()
                    .any(|suffix| upper.ends_with(suffix))
        {
            continue;
        }
        safe_path(Path::new("jar"), &name)?;
        let entry = archive.by_name(&name)?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("OptiFine JAR 不能有符号链接");
        }
        total = total
            .checked_add(entry.size())
            .context("OptiFine JAR 大小溢出")?;
        if entry.size() > 64 * 1024 * 1024 || total > 256 * 1024 * 1024 {
            bail!("OptiFine JAR 展开大小超限");
        }
        writer.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .last_modified_time(zip::DateTime::default())
                .unix_permissions(0o644),
        )?;
        let expected = entry.size();
        let mut written = 0u64;
        let mut entry = entry;
        let mut buffer = [0; 65536];
        loop {
            control.check()?;
            let count = entry.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            written += count as u64;
            actual_total += count as u64;
            if written > 64 * 1024 * 1024 || actual_total > 256 * 1024 * 1024 {
                bail!("OptiFine JAR 实际展开大小超限");
            }
            writer.write_all(&buffer[..count])?;
        }
        if written != expected {
            bail!("OptiFine JAR 条目大小与目录不一致");
        }
    }
    writer.finish()?.sync_all()?;
    control.check()
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    pub(crate) fn config(game: &str) -> Vec<u8> {
        let mut pool: Vec<Vec<u8>> = vec![];
        fn utf8(pool: &mut Vec<Vec<u8>>, s: &str) -> u16 {
            let mut b = vec![1];
            b.extend((s.len() as u16).to_be_bytes());
            b.extend(s.as_bytes());
            pool.push(b);
            pool.len() as u16
        }
        let class_name = utf8(&mut pool, "net/optifine/Config");
        pool.push([vec![7], class_name.to_be_bytes().to_vec()].concat());
        let this = pool.len() as u16;
        let object_name = utf8(&mut pool, "java/lang/Object");
        pool.push([vec![7], object_name.to_be_bytes().to_vec()].concat());
        let superclass = pool.len() as u16;
        let descriptor = utf8(&mut pool, "Ljava/lang/String;");
        let constant_value = utf8(&mut pool, "ConstantValue");
        let mut fields = vec![];
        for (name, value) in [
            ("MC_VERSION", game),
            ("OF_EDITION", "HD_U"),
            ("OF_RELEASE", "I6"),
        ] {
            let name = utf8(&mut pool, name);
            let value = utf8(&mut pool, value);
            pool.push([vec![8], value.to_be_bytes().to_vec()].concat());
            let value = pool.len() as u16;
            for number in [0x0019u16, name, descriptor, 1, constant_value] {
                fields.extend(number.to_be_bytes());
            }
            fields.extend(2u32.to_be_bytes());
            fields.extend(value.to_be_bytes());
        }
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52];
        bytes.extend(((pool.len() + 1) as u16).to_be_bytes());
        for entry in pool {
            bytes.extend(entry);
        }
        for value in [0x0021u16, this, superclass, 0, 3] {
            bytes.extend(value.to_be_bytes());
        }
        bytes.extend(fields);
        bytes.extend([0, 0, 0, 0]);
        bytes
    }
    pub(crate) fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(vec![]));
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
    pub(crate) fn installer(path: &Path, game: &str, patcher: bool, wrapper: bool) {
        let config = config(game);
        let header = [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52];
        let embedded = jar(&[("net/minecraft/launchwrapper/Launch.class", &header)]);
        let mut entries = vec![
            ("net/optifine/Config.class", config.as_slice()),
            ("optifine/OptiFineTweaker.class", header.as_slice()),
            ("optifine/OptiFineForgeTweaker.class", header.as_slice()),
            ("META-INF/mods.toml", b"must remove".as_slice()),
            ("buildof.txt", b"20211001-000000".as_slice()),
        ];
        if patcher {
            entries.push(("optifine/Patcher.class", header.as_slice()));
        }
        if wrapper {
            entries.push(("launchwrapper-of.txt", b"2.3"));
            entries.push(("launchwrapper-of-2.3.jar", embedded.as_slice()));
        }
        fs::write(path, jar(&entries)).unwrap();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_version_from_constant_value_fields_and_embedded_wrapper_not_filename() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("misleading-name.jar");
        fixtures::installer(&path, "1.20.1", true, true);
        let info = OptiFineArchive::inspect(&path, &TaskControl::default()).unwrap();
        assert_eq!(info.identity.game, "1.20.1");
        assert_eq!(info.identity.version(), "HD_U_I6");
        assert!(info.patcher && info.tweaker && info.forge_tweaker);
        assert_eq!(info.wrapper.unwrap().version, "2.3");
        assert!(config_constants(&fixtures::config("1.20.1")[..40]).is_err());
    }
    #[test]
    fn invalid_or_missing_identity_is_not_executable_install_input() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("bad.jar");
        fixtures::installer(&path, "../escape", false, false);
        assert!(OptiFineArchive::inspect(&path, &TaskControl::default()).is_err());
        fs::write(&path, fixtures::jar(&[("Config.class", b"not a class")])).unwrap();
        assert!(OptiFineArchive::inspect(&path, &TaskControl::default()).is_err());
        fs::write(&path, fixtures::jar(&[("anything", b"no identity")])).unwrap();
        assert!(OptiFineArchive::inspect(&path, &TaskControl::default()).is_err());
        let control = TaskControl::default();
        control.cancel();
        assert!(OptiFineArchive::inspect(Path::new("missing"), &control).is_err());
    }
    #[test]
    fn normalization_is_reproducible_and_removes_mod_registration_and_stale_signatures() {
        let workspace = crate::test_support::Workspace::new();
        let input = workspace.path.join("input.jar");
        let first = workspace.path.join("first.jar");
        let second = workspace.path.join("second.jar");
        fs::write(
            &input,
            fixtures::jar(&[
                ("z", b"last"),
                ("META-INF/mods.toml", b"mod"),
                ("META-INF/OLD.SF", b"signature"),
                ("a", b"first"),
            ]),
        )
        .unwrap();
        let control = TaskControl::default();
        normalize(&input, &first, &control).unwrap();
        normalize(&first, &second, &control).unwrap();
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        let archive = crate::installer_archive::open(&second, &control).unwrap();
        assert_eq!(archive.file_names().collect::<Vec<_>>(), ["a", "z"]);
        fs::write(&input, fixtures::jar(&[("../outside", b"bad")])).unwrap();
        assert!(normalize(&input, &first, &control).is_err());
    }
}
