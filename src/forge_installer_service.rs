use crate::{
    backend::Backend,
    download::Download,
    metadata::{safe_path, valid_id},
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

const MAX_PROFILE: u64 = 1024 * 1024;
const MAX_EMBEDDED: u64 = 128 * 1024 * 1024;
fn valid_hash(hash: &str) -> bool {
    hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}
fn archive_entry(
    archive: &mut zip::ZipArchive<fs::File>,
    name: &str,
    limit: u64,
    control: &TaskControl,
) -> Result<Vec<u8>> {
    safe_path(Path::new("installer"), name)?;
    if archive.file_names().filter(|entry| *entry == name).count() != 1 {
        bail!("安装包条目缺失或重复：{name}");
    }
    let mut entry = archive.by_name(name)?;
    if entry.is_dir()
        || entry.size() > limit
        || entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
    {
        bail!("安装包条目类型或大小无效：{name}");
    }
    let mut bytes = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        control.check()?;
        let count = entry.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if bytes.len() as u64 + count as u64 > limit {
            bail!("安装包条目超过读取限制：{name}");
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    control.check()?;
    Ok(bytes)
}
fn json_entry(
    archive: &mut zip::ZipArchive<fs::File>,
    name: &str,
    control: &TaskControl,
) -> Result<Value> {
    serde_json::from_slice(&archive_entry(archive, name, MAX_PROFILE, control)?)
        .with_context(|| format!("安装描述格式错误：{name}"))
}
fn validate_profile(profile: &Value, game: &str) -> Result<String> {
    let id = profile["id"].as_str().context("安装器缺少客户端版本 ID")?;
    valid_id(id)?;
    if id == game {
        bail!("加载器实例 ID 不能覆盖原版父版本");
    }
    if let Some(parent) = profile.get("inheritsFrom").filter(|value| !value.is_null()) {
        if parent.as_str() != Some(game) {
            bail!("官方安装器的父版本与选择不一致");
        }
    }
    Ok(id.into())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallerSource {
    pub(crate) url: String,
    pub(crate) sha1: String,
}
impl InstallerSource {
    fn validate(&self) -> Result<()> {
        crate::metadata_cache::validate_url(&self.url)?;
        if !valid_hash(&self.sha1) {
            bail!("安装器来源缺少有效 SHA-1");
        }
        Ok(())
    }
    pub(crate) fn cache_path(&self, root: &Path) -> Result<PathBuf> {
        self.validate()?;
        Ok(root
            .join("tui-mc-installers")
            .join(format!("{}.jar", self.sha1.to_ascii_lowercase())))
    }
    fn universal_url(&self) -> Result<String> {
        self.validate()?;
        let mut url = reqwest::Url::parse(&self.url)?;
        let path = url
            .path()
            .strip_suffix("-installer.jar")
            .context("无法识别旧 Forge 安装器来源文件名")?;
        url.set_path(&format!("{path}-universal.jar"));
        Ok(url.into())
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EmbeddedReference {
    source: InstallerSource,
    entry: String,
}
pub(crate) struct DirectForgePlan {
    profile: Value,
    components: Vec<EmbeddedComponent>,
}
struct EmbeddedComponent {
    coordinate: String,
    entry: String,
    sha1: String,
    size: u64,
    url: Option<String>,
}
pub(crate) struct ForgeInstallerPlan {
    pub(crate) id: String,
    pub(crate) direct: Option<DirectForgePlan>,
    pub(crate) output: Option<crate::loader_output_service::LoaderOutputRequirement>,
}
impl ForgeInstallerPlan {
    pub(crate) fn inspect(
        path: &Path,
        game: &str,
        full: Option<&str>,
        control: &TaskControl,
    ) -> Result<Self> {
        valid_id(game)?;
        control.check()?;
        let mut archive = crate::installer_archive::open(path, control)?;
        let install = json_entry(&mut archive, "install_profile.json", control)?;
        if install
            .get("minecraft")
            .filter(|value| !value.is_null())
            .is_some_and(|value| value.as_str() != Some(game))
        {
            bail!("Forge 安装器的 Minecraft 版本不匹配");
        }
        if install.get("install").is_some() || install.get("versionInfo").is_some() {
            let details = install["install"]
                .as_object()
                .context("旧 Forge 安装描述缺少 install")?;
            if let Some(minecraft) = details.get("minecraft").filter(|value| !value.is_null()) {
                if minecraft.as_str() != Some(game) {
                    bail!("旧 Forge 安装器的 Minecraft 版本不匹配");
                }
            }
            let coordinate = details
                .get("path")
                .and_then(Value::as_str)
                .context("旧 Forge 缺少 Maven 坐标")?;
            let parts: Vec<_> = coordinate.split(':').collect();
            crate::version::maven_path(coordinate)?;
            if !(3..=4).contains(&parts.len())
                || parts[0] != "net.minecraftforge"
                || !matches!(parts[1], "forge" | "minecraftforge")
                || full.is_some_and(|full| {
                    parts[2] != full && Some(parts[2]) != full.strip_prefix(&format!("{game}-"))
                })
            {
                bail!("旧 Forge 内置库坐标与所选版本不一致");
            }
            let entry = details
                .get("filePath")
                .and_then(Value::as_str)
                .context("旧 Forge 缺少内置 JAR 路径")?;
            let bytes = archive_entry(&mut archive, entry, MAX_EMBEDDED, control)?;
            zip::ZipArchive::new(Cursor::new(&bytes)).context("内置 Forge 库不是有效 JAR")?;
            let mut profile = install["versionInfo"].clone();
            let id = validate_profile(&profile, game)?;
            if !profile["mainClass"].as_str().is_some_and(|main| {
                !main.is_empty() && !main.chars().any(|c| c.is_control() || c.is_whitespace())
            }) {
                bail!("旧 Forge 版本描述缺少有效启动入口");
            }
            if !profile["libraries"].is_array() {
                bail!("旧 Forge 版本描述缺少 libraries");
            }
            if profile
                .get("jar")
                .filter(|value| !value.is_null())
                .is_some_and(|value| value.as_str() != Some(game))
            {
                bail!("旧 Forge 客户端归属与父版本不一致，不能直接安装");
            }
            if profile
                .pointer("/downloads/client")
                .is_some_and(|value| !value.is_null())
            {
                bail!("此旧 Forge 要求替换客户端，暂不能按 universal JAR 方式安装");
            }
            profile["inheritsFrom"] = game.into();
            profile["jar"] = game.into();
            return Ok(Self {
                id,
                output: None,
                direct: Some(DirectForgePlan {
                    profile,
                    components: vec![EmbeddedComponent {
                        coordinate: coordinate.into(),
                        entry: entry.into(),
                        sha1: format!("{:x}", Sha1::digest(&bytes)),
                        size: bytes.len() as u64,
                        url: None,
                    }],
                }),
            });
        }
        let entry = match install.get("json") {
            Some(Value::String(name)) => name.strip_prefix('/').unwrap_or(name),
            None => "version.json",
            Some(_) => bail!("现代 Forge 的 json 路径无效"),
        };
        let mut profile = json_entry(&mut archive, entry, control)?;
        let id = validate_profile(&profile, game)?;
        if install["spec"].as_u64() == Some(0) {
            let processors = match install.get("processors").filter(|value| !value.is_null()) {
                None => 0,
                Some(value) => value
                    .as_array()
                    .context("Forge processors 必须为数组")?
                    .len(),
            };
            // Spec 0 packages without processors are PCL's intermediate legacy
            // format: their ready-to-use libraries are under maven/, not install/.
            if processors == 0 {
                if !profile["mainClass"].as_str().is_some_and(|main| {
                    !main.is_empty() && !main.chars().any(|c| c.is_control() || c.is_whitespace())
                }) || !profile["libraries"].is_array()
                {
                    bail!("可直接安装的 Forge 描述缺少启动入口或依赖");
                }
                if profile
                    .get("jar")
                    .filter(|value| !value.is_null())
                    .is_some_and(|value| value.as_str() != Some(game))
                    || profile
                        .pointer("/downloads/client")
                        .is_some_and(|value| !value.is_null())
                {
                    bail!("此 Forge 描述要求替换客户端，不能直接安装");
                }
                let mut components = vec![];
                let mut total = 0u64;
                let mut core = false;
                for library in profile["libraries"].as_array().unwrap() {
                    control.check()?;
                    let coordinate = library["name"].as_str().context("Forge 依赖缺少坐标")?;
                    let path = crate::version::maven_path(coordinate)?;
                    let entry = format!("maven/{path}");
                    if !archive.file_names().any(|name| name == entry) {
                        continue;
                    }
                    if coordinate.starts_with("net.minecraftforge:forge:") {
                        if full.is_some_and(|full| coordinate.split(':').nth(2) != Some(full)) {
                            bail!("内置 Forge 库与所选版本不一致");
                        }
                        core = true;
                    }
                    let bytes = archive_entry(&mut archive, &entry, MAX_EMBEDDED, control)?;
                    total += bytes.len() as u64;
                    if total > MAX_EMBEDDED * 2 || components.len() >= 1024 {
                        bail!("Forge 内置库总量超过限制");
                    }
                    zip::ZipArchive::new(Cursor::new(&bytes))
                        .context("内置 Maven 库不是有效 JAR")?;
                    let url = library
                        .pointer("/downloads/artifact/url")
                        .and_then(Value::as_str)
                        .filter(|url| !url.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("https://maven.minecraftforge.net/{path}"));
                    crate::metadata_cache::validate_url(&url)?;
                    components.push(EmbeddedComponent {
                        coordinate: coordinate.into(),
                        entry,
                        sha1: format!("{:x}", Sha1::digest(&bytes)),
                        size: bytes.len() as u64,
                        url: Some(url),
                    });
                }
                if !core {
                    bail!("可直接安装的 Forge 包缺少所需内置核心库");
                }
                profile["inheritsFrom"] = game.into();
                profile["jar"] = game.into();
                return Ok(Self {
                    id,
                    output: None,
                    direct: Some(DirectForgePlan {
                        profile,
                        components,
                    }),
                });
            }
        }
        let output = crate::loader_output_service::LoaderOutputRequirement::new(&profile, game)?;
        Ok(Self {
            id,
            direct: None,
            output: Some(output),
        })
    }
}
impl DirectForgePlan {
    pub(crate) fn profile(mut self, source: InstallerSource) -> Result<Value> {
        source.validate()?;
        for component in self.components {
            let url = match component.url {
                Some(url) => url,
                None => source.universal_url()?,
            };
            let artifact = json!({"path":crate::version::maven_path(&component.coordinate)?,"url":url,"sha1":component.sha1,"size":component.size});
            let libraries = self.profile["libraries"]
                .as_array_mut()
                .context("旧 Forge 缺少依赖列表")?;
            let indexes: Vec<_> = libraries
                .iter()
                .enumerate()
                .filter(|(_, lib)| lib["name"] == component.coordinate)
                .map(|(index, _)| index)
                .collect();
            if indexes.len() > 1 {
                bail!("旧 Forge 版本描述包含重复核心库");
            }
            let index = match indexes.first() {
                Some(index) => *index,
                None => {
                    libraries.insert(0, json!({"name":component.coordinate}));
                    0
                }
            };
            if !libraries[index]["natives"].is_null() {
                bail!("Forge 核心库不能声明为 native");
            }
            libraries[index]["downloads"] = json!({"artifact":artifact});
            libraries[index]["tuiMcEmbedded"] = serde_json::to_value(EmbeddedReference {
                source: source.clone(),
                entry: component.entry,
            })?;
        }
        Ok(self.profile)
    }
}

/// Provides a verified archive; policy and archive IO never perform HTTP directly.
pub(crate) trait InstallerArchiveSource {
    fn prepare(&self, root: &Path, source: &InstallerSource) -> Result<PathBuf>;
}
impl InstallerArchiveSource for Backend {
    fn prepare(&self, root: &Path, source: &InstallerSource) -> Result<PathBuf> {
        let dest = source.cache_path(root)?;
        self.downloader().fetch(&Download {
            url: source.url.clone(),
            dest: dest.clone(),
            sha1: Some(source.sha1.clone()),
            size: None,
            alternatives: vec![],
        })?;
        Ok(dest)
    }
}
pub(crate) struct InstallerArtifactService<'a> {
    source: &'a dyn InstallerArchiveSource,
    control: &'a TaskControl,
}
impl<'a> InstallerArtifactService<'a> {
    pub(crate) fn new(source: &'a dyn InstallerArchiveSource, control: &'a TaskControl) -> Self {
        Self { source, control }
    }
    pub(crate) fn prepare(&self, root: &Path, library: &Value) -> Result<()> {
        self.control.check()?;
        let Some(reference) = library.get("tuiMcEmbedded") else {
            return Ok(());
        };
        let reference: EmbeddedReference =
            serde_json::from_value(reference.clone()).context("内置库恢复描述无效")?;
        reference.source.validate()?;
        safe_path(Path::new("installer"), &reference.entry)?;
        let entry = &library["downloads"]["artifact"];
        let hash = entry["sha1"]
            .as_str()
            .filter(|hash| valid_hash(hash))
            .context("内置库缺少有效 SHA-1")?;
        let size = entry["size"]
            .as_u64()
            .filter(|size| *size > 0 && *size <= MAX_EMBEDDED)
            .context("内置库大小无效")?;
        let destination = safe_path(
            &root.join("libraries"),
            entry["path"].as_str().context("内置库缺少目标路径")?,
        )?;
        let cached = || -> Result<bool> {
            Ok(
                crate::download::valid_file(&destination, Some(hash), Some(size))?
                    && zip::ZipArchive::new(fs::File::open(&destination)?).is_ok(),
            )
        };
        if cached()? {
            return Ok(());
        }
        let archive = self.source.prepare(root, &reference.source)?;
        if !crate::download::valid_file(&archive, Some(&reference.source.sha1), None)? {
            bail!("内置库来源安装器校验失败");
        }
        let mut archive = crate::installer_archive::open(&archive, self.control)?;
        let bytes = archive_entry(&mut archive, &reference.entry, MAX_EMBEDDED, self.control)?;
        if bytes.len() as u64 != size
            || format!("{:x}", Sha1::digest(&bytes)) != hash.to_ascii_lowercase()
        {
            bail!("内置库校验失败，原依赖未修改");
        }
        zip::ZipArchive::new(Cursor::new(&bytes)).context("内置库不是有效 JAR")?;
        let _lease =
            crate::download_lock::DownloadLease::acquire(&destination, self.control, || {})?;
        if cached()? {
            return Ok(());
        }
        self.control.check()?;
        crate::storage::atomic_write(&destination, &bytes).context("发布内置依赖失败，原文件已保留")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, io::Write};
    const GAME: &str = "1.12.2";
    const FULL: &str = "1.12.2-14.23.5.2859";
    const ID: &str = "legacy-forge";
    const COORD: &str = "net.minecraftforge:forge:1.12.2-14.23.5.2859";
    fn jar() -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(vec![]));
        archive
            .start_file("fixture", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(b"local fixture, no Java execution")
            .unwrap();
        archive.finish().unwrap().into_inner()
    }
    fn old() -> Value {
        json!({"install":{"minecraft":GAME,"path":COORD,"filePath":"forge-universal.jar"},
            "versionInfo":{"id":ID,"mainClass":"net.minecraft.launchwrapper.Launch",
            "minecraftArguments":"--tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker",
            "libraries":[{"name":COORD,"url":"https://maven.minecraftforge.net/"}]}})
    }
    fn write_archive(path: &Path, entries: &[(&str, &[u8])]) {
        let mut archive = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, bytes) in entries {
            archive
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap();
    }
    fn installer(path: &Path, profile: &Value) -> InstallerSource {
        write_archive(
            path,
            &[
                (
                    "install_profile.json",
                    &serde_json::to_vec(profile).unwrap(),
                ),
                ("forge-universal.jar", &jar()),
            ],
        );
        InstallerSource {url:format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{FULL}/forge-{FULL}-installer.jar"),
            sha1:format!("{:x}",Sha1::digest(fs::read(path).unwrap()))}
    }
    fn legacy_profile(path: &Path, source: InstallerSource) -> Value {
        ForgeInstallerPlan::inspect(path, GAME, Some(FULL), &TaskControl::default())
            .unwrap()
            .direct
            .unwrap()
            .profile(source)
            .unwrap()
    }
    struct Source {
        path: PathBuf,
        calls: Cell<usize>,
    }
    impl InstallerArchiveSource for Source {
        fn prepare(&self, _: &Path, _: &InstallerSource) -> Result<PathBuf> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.path.clone())
        }
    }
    #[test]
    fn old_profile_inherits_parent_and_keeps_tweaker_with_a_pinned_embedded_library() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("installer.jar");
        let source = installer(&path, &old());
        let profile = legacy_profile(&path, source.clone());
        assert_eq!(profile["inheritsFrom"], GAME);
        assert_eq!(profile["jar"], GAME);
        assert!(profile["minecraftArguments"]
            .as_str()
            .unwrap()
            .contains("FMLTweaker"));
        let library = &profile["libraries"][0];
        assert_eq!(
            library["downloads"]["artifact"]["sha1"],
            format!("{:x}", Sha1::digest(jar()))
        );
        assert!(library["downloads"]["artifact"]["url"]
            .as_str()
            .unwrap()
            .ends_with("-universal.jar"));
        assert_eq!(library["tuiMcEmbedded"]["source"]["sha1"], source.sha1);
        assert_eq!(
            crate::library_service::LibraryResolver::current()
                .resolve(library)
                .unwrap()
                .len(),
            1
        );
        let mut missing = old();
        missing["versionInfo"]["libraries"] = json!([]);
        let source = installer(&path, &missing);
        assert_eq!(
            legacy_profile(&path, source)["libraries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn modern_profile_uses_its_declared_json_entry_without_running_an_installer() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("modern.jar");
        write_archive(
            &path,
            &[
                (
                    "install_profile.json",
                    br#"{"spec":1,"json":"/data/client.json"}"#,
                ),
                (
                    "data/client.json",
                    br#"{"id":"modern-forge","inheritsFrom":"1.12.2","mainClass":"example.ForgeMain","libraries":[]}"#,
                ),
            ],
        );
        let plan = ForgeInstallerPlan::inspect(&path, GAME, None, &TaskControl::default()).unwrap();
        assert_eq!(plan.id, "modern-forge");
        assert!(plan.direct.is_none());
        write_archive(
            &path,
            &[(
                "install_profile.json",
                br#"{"spec":1,"json":"../escape.json"}"#,
            )],
        );
        assert!(ForgeInstallerPlan::inspect(&path, GAME, None, &TaskControl::default()).is_err());
    }
    #[test]
    fn rejects_wrong_parent_identity_unsafe_entry_missing_core_and_bad_universal() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("bad.jar");
        let mut cases = vec![];
        for (field, value) in [
            ("minecraft", json!("1.7.10")),
            ("path", json!("evil:forge:1")),
            ("path", json!("net.minecraftforge:forge:wrong")),
            ("filePath", json!("../escape.jar")),
        ] {
            let mut profile = old();
            profile["install"][field] = value;
            cases.push(profile);
        }
        for (field, value) in [
            ("inheritsFrom", json!("1.7.10")),
            ("id", json!(GAME)),
            ("jar", json!("unrelated")),
            ("mainClass", json!("")),
            ("libraries", json!(null)),
        ] {
            let mut profile = old();
            profile["versionInfo"][field] = value;
            cases.push(profile);
        }
        for profile in cases {
            installer(&path, &profile);
            assert!(
                ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default())
                    .is_err(),
                "{profile}"
            );
        }
        write_archive(
            &path,
            &[
                ("install_profile.json", &serde_json::to_vec(&old()).unwrap()),
                ("forge-universal.jar", b"not a jar"),
            ],
        );
        assert!(
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default()).is_err()
        );
        write_archive(
            &path,
            &[("install_profile.json", &serde_json::to_vec(&old()).unwrap())],
        );
        assert!(
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default()).is_err()
        );
    }
    #[test]
    fn bounds_metadata_and_does_not_fall_back_from_corrupt_modern_json() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("bad.jar");
        write_archive(
            &path,
            &[(
                "install_profile.json",
                &vec![b' '; MAX_PROFILE as usize + 1],
            )],
        );
        assert!(ForgeInstallerPlan::inspect(&path, GAME, None, &TaskControl::default()).is_err());
        write_archive(
            &path,
            &[
                ("install_profile.json", br#"{"spec":1}"#),
                ("version.json", b"broken JSON"),
            ],
        );
        assert!(ForgeInstallerPlan::inspect(&path, GAME, None, &TaskControl::default()).is_err());
        let control = TaskControl::default();
        control.cancel();
        assert!(ForgeInstallerPlan::inspect(&path, GAME, None, &control)
            .err()
            .unwrap()
            .to_string()
            .contains("取消"));
    }
    #[test]
    fn intermediate_spec_zero_restores_declared_maven_libraries_without_processors() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("intermediate.jar");
        let entry = format!("maven/{}", crate::version::maven_path(COORD).unwrap());
        let description = old()["versionInfo"].clone();
        let metadata = json!({"spec":0,"minecraft":GAME,"json":"/version.json","processors":[]});
        write_archive(
            &path,
            &[
                (
                    "install_profile.json",
                    &serde_json::to_vec(&metadata).unwrap(),
                ),
                ("version.json", &serde_json::to_vec(&description).unwrap()),
                (&entry, &jar()),
            ],
        );
        let source = InstallerSource {
            url: format!("https://maven.minecraftforge.net/forge-{FULL}-installer.jar"),
            sha1: format!("{:x}", Sha1::digest(fs::read(&path).unwrap())),
        };
        let plan =
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default()).unwrap();
        let profile = plan.direct.unwrap().profile(source).unwrap();
        assert_eq!(profile["inheritsFrom"], GAME);
        assert_eq!(profile["libraries"][0]["tuiMcEmbedded"]["entry"], entry);
        assert_eq!(
            profile["libraries"][0]["downloads"]["artifact"]["sha1"],
            format!("{:x}", Sha1::digest(jar()))
        );
        let mut active = metadata;
        active["processors"] = json!([{"jar":"test:processor:1"}]);
        write_archive(
            &path,
            &[
                (
                    "install_profile.json",
                    &serde_json::to_vec(&active).unwrap(),
                ),
                ("version.json", &serde_json::to_vec(&description).unwrap()),
            ],
        );
        assert!(
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default())
                .unwrap()
                .direct
                .is_none()
        );
    }
    #[test]
    fn intermediate_format_refuses_missing_core_or_a_mismatched_game() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("intermediate.jar");
        let metadata = json!({"spec":0,"minecraft":GAME,"json":"version.json"});
        write_archive(
            &path,
            &[
                (
                    "install_profile.json",
                    &serde_json::to_vec(&metadata).unwrap(),
                ),
                (
                    "version.json",
                    &serde_json::to_vec(&old()["versionInfo"]).unwrap(),
                ),
            ],
        );
        assert!(
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default()).is_err()
        );
        let mut metadata = metadata;
        metadata["minecraft"] = "1.7.10".into();
        write_archive(
            &path,
            &[
                (
                    "install_profile.json",
                    &serde_json::to_vec(&metadata).unwrap(),
                ),
                (
                    "version.json",
                    &serde_json::to_vec(&old()["versionInfo"]).unwrap(),
                ),
            ],
        );
        assert!(
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default()).is_err()
        );
    }
    #[test]
    fn historical_minecraftforge_coordinates_can_omit_the_game_prefix() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("historical.jar");
        let mut profile = old();
        let coordinate = "net.minecraftforge:minecraftforge:14.23.5.2859";
        profile["install"]["path"] = coordinate.into();
        profile["versionInfo"]["libraries"][0]["name"] = coordinate.into();
        let source = installer(&path, &profile);
        let plan =
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default()).unwrap();
        let profile = plan.direct.unwrap().profile(source).unwrap();
        assert_eq!(profile["libraries"][0]["name"], coordinate);
    }
    #[test]
    fn refuses_duplicate_core_libraries_and_unpinned_installer_sources() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("installer.jar");
        let mut profile = old();
        profile["versionInfo"]["libraries"] = json!([{"name":COORD},{"name":COORD}]);
        let source = installer(&path, &profile);
        let plan =
            ForgeInstallerPlan::inspect(&path, GAME, Some(FULL), &TaskControl::default()).unwrap();
        assert!(plan.direct.unwrap().profile(source).is_err());
        for source in [
            InstallerSource {
                url: "http://unsafe.invalid/a-installer.jar".into(),
                sha1: "a".repeat(40),
            },
            InstallerSource {
                url: "https://safe.invalid/a-installer.jar".into(),
                sha1: "../escape".into(),
            },
        ] {
            assert!(source.cache_path(&workspace.path).is_err());
        }
    }
    #[test]
    #[ignore = "requires cached official installer fixtures; read-only, never runs Java"]
    fn cached_old_and_modern_official_installers_use_their_actual_package_structure() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let fixtures = [
            (
                ".tools/forge-fixtures/forge-1.7.10-10.13.4.1614-1.7.10-installer.jar",
                "1.7.10",
                Some("1.7.10-10.13.4.1614-1.7.10"),
                true,
            ),
            (
                ".tools/forge-fixtures/forge-1.12.2-14.23.5.2859-installer.jar",
                "1.12.2",
                Some(FULL),
                true,
            ),
            (
                ".tools/smoke-game/tui-mc-installers/forge-1.21.1-52.1.20-installer.jar",
                "1.21.1",
                Some("1.21.1-52.1.20"),
                false,
            ),
            (
                ".tools/smoke-game/tui-mc-installers/neoforge-21.1.264-installer.jar",
                "1.21.1",
                None,
                false,
            ),
        ];
        for (path, game, full, legacy) in fixtures {
            let path = root.join(path);
            let plan =
                ForgeInstallerPlan::inspect(&path, game, full, &TaskControl::default()).unwrap();
            assert_eq!(plan.direct.is_some(), legacy, "{}", path.display());
            if let Some(legacy) = plan.direct {
                let profile = legacy
                    .profile(InstallerSource {
                        url: format!(
                            "https://maven.minecraftforge.net/net/minecraftforge/forge/{}/{}",
                            full.unwrap(),
                            path.file_name().unwrap().to_string_lossy()
                        ),
                        sha1: format!("{:x}", Sha1::digest(fs::read(&path).unwrap())),
                    })
                    .unwrap();
                assert_eq!(profile["inheritsFrom"], game);
                assert!(profile["libraries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|lib| lib.get("tuiMcEmbedded").is_some()));
            } else if let Some(output) = plan.output {
                output
                    .verify(&root.join(".tools/smoke-game"), &TaskControl::default())
                    .unwrap();
            }
        }
    }
    #[test]
    fn embedded_restore_checks_source_and_payload_before_replacing_existing_files() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("installer.jar");
        let reference = installer(&path, &old());
        let profile = legacy_profile(&path, reference.clone());
        let source = Source {
            path: path.clone(),
            calls: Cell::new(0),
        };
        let control = TaskControl::default();
        let service = InstallerArtifactService::new(&source, &control);
        let mut library = profile["libraries"][0].clone();
        let destination = workspace
            .path
            .join("libraries")
            .join(crate::version::maven_path(COORD).unwrap());
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"original content").unwrap();
        library["downloads"]["artifact"]["sha1"] = "0".repeat(40).into();
        assert!(service.prepare(&workspace.path, &library).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original content");
        library = profile["libraries"][0].clone();
        library["tuiMcEmbedded"]["source"]["sha1"] = "0".repeat(40).into();
        assert!(service.prepare(&workspace.path, &library).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"original content");
        control.cancel();
        let calls = source.calls.get();
        assert!(service
            .prepare(&workspace.path, &profile["libraries"][0])
            .is_err());
        assert_eq!(source.calls.get(), calls);
    }
    #[test]
    fn embedded_restore_repairs_corruption_then_reuses_valid_file_without_source_access() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("installer.jar");
        let reference = installer(&path, &old());
        let profile = legacy_profile(&path, reference);
        let source = Source {
            path: path.clone(),
            calls: Cell::new(0),
        };
        let control = TaskControl::default();
        let service = InstallerArtifactService::new(&source, &control);
        let library = &profile["libraries"][0];
        service.prepare(&workspace.path, library).unwrap();
        let destination = workspace
            .path
            .join("libraries")
            .join(crate::version::maven_path(COORD).unwrap());
        assert_eq!(fs::read(&destination).unwrap(), jar());
        fs::write(&destination, b"damaged").unwrap();
        service.prepare(&workspace.path, library).unwrap();
        assert_eq!(source.calls.get(), 2);
        fs::remove_file(&path).unwrap();
        service.prepare(&workspace.path, library).unwrap();
        assert_eq!(source.calls.get(), 2);
    }
    #[test]
    fn backend_installs_and_repairs_legacy_forge_from_cached_installer_without_java_or_network() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        let path = root.join("installer.jar");
        let reference = installer(&path, &old());
        let profile = legacy_profile(&path, reference.clone());
        let cached = reference.cache_path(root).unwrap();
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        fs::copy(&path, &cached).unwrap();
        let client = jar();
        let base = root.join("versions").join(GAME);
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join(format!("{GAME}.jar")), &client).unwrap();
        let assets = br#"{"objects":{}}"#;
        fs::create_dir_all(root.join("assets/indexes")).unwrap();
        fs::write(root.join("assets/indexes/fixture.json"), assets).unwrap();
        fs::write(base.join(format!("{GAME}.json")),serde_json::to_vec(&json!({"id":GAME,"mainClass":"net.minecraft.client.main.Main","libraries":[],
            "downloads":{"client":{"url":"https://fixture.invalid/client.jar","sha1":format!("{:x}",Sha1::digest(&client)),"size":client.len()}},
            "assetIndex":{"id":"fixture","url":"https://fixture.invalid/index.json","sha1":format!("{:x}",Sha1::digest(assets)),"size":assets.len()}})).unwrap()).unwrap();
        let config = crate::config::Config {
            game_dir: root.clone(),
            ..Default::default()
        };
        let canonical_root = config.root().unwrap();
        let root = &canonical_root;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut backend = Backend::new(tx).unwrap();
        backend.client = reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();
        {
            let mut mutation = crate::instance_service::InstanceMutation::begin(root, ID).unwrap();
            backend
                .install_profile_with_session(&config, ID, profile.clone(), &mut mutation)
                .unwrap();
        }
        let installed = root.join("versions").join(ID).join(format!("{ID}.json"));
        let previous = fs::read(&installed).unwrap();
        let resolved = crate::version::resolve(root, ID).unwrap();
        assert_eq!(resolved.jar_id, GAME);
        assert_eq!(
            resolved.meta["mainClass"],
            "net.minecraft.launchwrapper.Launch"
        );
        let library = root
            .join("libraries")
            .join(crate::version::maven_path(COORD).unwrap());
        fs::write(&library, b"corrupt").unwrap();
        backend.repair(&config, ID).unwrap();
        assert_eq!(fs::read(&library).unwrap(), jar());
        assert_eq!(fs::read(&installed).unwrap(), previous);
        let mut bad = profile;
        bad["libraries"][0]["downloads"]["artifact"]["sha1"] = "0".repeat(40).into();
        let mut mutation = crate::instance_service::InstanceMutation::begin(root, ID).unwrap();
        assert!(backend
            .install_profile_with_session(&config, ID, bad, &mut mutation)
            .is_err());
        assert_eq!(fs::read(&installed).unwrap(), previous);
        assert_eq!(fs::read(&library).unwrap(), jar());
    }
}
