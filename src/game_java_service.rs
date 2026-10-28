use crate::{
    metadata::valid_id, runtime_service::JavaRequirement, task::TaskControl,
    version::ResolvedVersion,
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{fs, io::Read, path::Path};

#[derive(Default)]
pub(crate) struct ClientJavaInfo {
    pub(crate) version: Option<String>,
    pub(crate) java: Option<u64>,
    pub(crate) component: Option<String>,
    pub(crate) class_java: Option<u64>,
}
pub(crate) trait ClientVersionSource {
    fn read(&self, jar: &Path, control: &TaskControl) -> Result<ClientJavaInfo>;
}
pub(crate) struct JarClientVersionSource;
impl ClientVersionSource for JarClientVersionSource {
    fn read(&self, jar: &Path, control: &TaskControl) -> Result<ClientJavaInfo> {
        control.check()?;
        let file = match fs::File::open(jar) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ClientJavaInfo::default())
            }
            Err(error) => return Err(error).context("无法读取客户端版本信息"),
        };
        let mut archive = crate::installer_archive::open_file(file, control)
            .with_context(|| format!("客户端 JAR 无效，请修复实例：{}", jar.display()))?;
        let mut info = ClientJavaInfo::default();
        match archive.by_name("version.json") {
            Ok(mut entry) => {
                const LIMIT: u64 = 1024 * 1024;
                if entry.size() > LIMIT {
                    bail!("客户端 version.json 超出读取限制");
                }
                let mut data = Vec::new();
                (&mut entry).take(LIMIT + 1).read_to_end(&mut data)?;
                control.check()?;
                if data.len() as u64 > LIMIT {
                    bail!("客户端 version.json 超出读取限制");
                }
                let value: Value = serde_json::from_slice(&data)
                    .context("客户端 version.json 格式错误，请修复实例")?;
                if !value.is_object() {
                    bail!("客户端 version.json 必须是对象");
                }
                info.version = optional_string(&value, "id")?
                    .map(|id| id.split(" / ").next().unwrap_or(id).into());
                info.java = match value.get("java_version") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_u64()
                            .filter(|major| *major > 0)
                            .context("客户端 java_version 必须为正整数")?,
                    ),
                };
                info.component = optional_string(&value, "java_component")?.map(str::to_owned);
            }
            Err(zip::result::ZipError::FileNotFound) => {}
            Err(error) => return Err(error).context("读取客户端 version.json 失败"),
        }
        control.check()?;
        // The vanilla entry point is independent of a loader's own entry point.
        match archive.by_name("net/minecraft/client/main/Main.class") {
            Ok(mut entry) => {
                let mut header = [0; 8];
                entry
                    .read_exact(&mut header)
                    .context("客户端主类文件不完整，请修复实例")?;
                if header[..4] != [0xca, 0xfe, 0xba, 0xbe] {
                    bail!("客户端主类不是有效的 Java class 文件");
                }
                let class_version = u16::from_be_bytes([header[6], header[7]]);
                if class_version < 45 {
                    bail!("客户端主类字节码版本无效");
                }
                info.class_java = Some(u64::from(class_version - 44));
            }
            Err(zip::result::ZipError::FileNotFound) => {}
            Err(error) => return Err(error).context("读取客户端主类版本失败"),
        }
        control.check()?;
        Ok(info)
    }
}

fn optional_string<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(
            value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .with_context(|| format!("{key} 必须为非空字符串"))?,
        )),
    }
}

fn component_java(component: Option<&str>) -> Option<u64> {
    match component {
        Some("jre-legacy") => Some(8),
        Some("java-runtime-alpha") => Some(16),
        Some("java-runtime-beta" | "java-runtime-gamma") => Some(17),
        Some("java-runtime-delta") => Some(21),
        Some("java-runtime-epsilon") => Some(25),
        _ => None,
    }
}
fn check_component(component: Option<&str>, java: u64) -> Result<()> {
    if let Some(known) = component_java(component).filter(|known| *known != java) {
        bail!(
            "Java 组件 {} 提供 Java {known}，但实例要求 Java {java}；请修正版本配置",
            component.unwrap()
        );
    }
    Ok(())
}

pub(crate) struct GameJavaPlan {
    pub(crate) requirement: JavaRequirement,
    pub(crate) component: Option<String>,
    pub(crate) origin: &'static str,
}
/// Keeps version recognition / fallback policy separate from archive IO.
pub(crate) struct GameJavaResolver<'a> {
    source: &'a dyn ClientVersionSource,
}
impl<'a> GameJavaResolver<'a> {
    pub(crate) fn new(source: &'a dyn ClientVersionSource) -> Self {
        Self { source }
    }
    pub(crate) fn resolve(
        &self,
        root: &Path,
        version: &ResolvedVersion,
        control: &TaskControl,
    ) -> Result<GameJavaPlan> {
        control.check()?;
        valid_id(&version.jar_id)?;
        let meta = &version.meta;
        if meta
            .get("javaVersion")
            .is_some_and(|v| !v.is_null() && !v.is_object())
        {
            bail!("javaVersion 必须是对象");
        }
        let component = meta
            .get("javaVersion")
            .map(|value| optional_string(value, "component"))
            .transpose()?
            .flatten();
        if let Some(component) = component {
            valid_id(component)?;
        }
        if meta.pointer("/javaVersion/majorVersion").is_some() {
            let requirement = JavaRequirement::from_metadata(meta)?;
            let declared = meta["javaVersion"]["majorVersion"]
                .as_u64()
                .context("无效的 Java 版本")?;
            check_component(component, declared)?;
            let raised = requirement.recommended() > declared;
            return Ok(GameJavaPlan {
                requirement,
                component: if raised {
                    None
                } else {
                    component.map(str::to_owned)
                },
                origin: if raised {
                    "加载器 Java 要求"
                } else {
                    "版本元数据"
                },
            });
        }
        let jar = root
            .join("versions")
            .join(&version.jar_id)
            .join(format!("{}.jar", version.jar_id));
        let loaded = self.source.read(&jar, control);
        control.check()?;
        let info = loaded?;
        let component = component.map(str::to_owned).or(info.component);
        let (mut major, mut origin) = if let Some(major) = info.java {
            (major, "客户端 version.json")
        } else if let Some(major) = info.version.as_deref().and_then(release_java) {
            (major, "客户端内置版本号")
        } else if let Some(major) = version.chain.first().and_then(|v| release_java(v)) {
            (major, "原版继承版本号")
        } else if let Some(major) = component_java(component.as_deref()) {
            (major, "Java 组件")
        } else if let Some(major) = meta["releaseTime"].as_str().and_then(release_date_java) {
            (major, "版本发布时间")
        } else {
            (8, "兼容默认值")
        };
        if let Some(class_java) = info.class_java.filter(|class_java| *class_java > major) {
            major = class_java;
            origin = "客户端字节码版本";
        }
        if let Some(component_java) =
            component_java(component.as_deref()).filter(|known| *known > major)
        {
            major = component_java;
            origin = "Java 组件";
        }
        if let Some(component) = &component {
            valid_id(component)?;
        }
        check_component(component.as_deref(), major)?;
        let mut effective = meta.clone();
        if !effective["javaVersion"].is_object() {
            effective["javaVersion"] = serde_json::json!({});
        }
        effective["javaVersion"]["majorVersion"] = major.into();
        let requirement = JavaRequirement::from_metadata(&effective)?;
        let raised = requirement.recommended() > major;
        Ok(GameJavaPlan {
            requirement,
            component: if raised { None } else { component },
            origin: if raised {
                "加载器 Java 要求"
            } else {
                origin
            },
        })
    }
}

// Only exact vanilla release IDs are inferred; editable child instance names
// and suffixes such as '-my-pack' are not searched for embedded version text.
fn release_java(version: &str) -> Option<u64> {
    if !version.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
        return None;
    }
    let numbers: Vec<u32> = version
        .split('.')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    match numbers.as_slice() {
        [year, minor] | [year, minor, _] if (*year, *minor) >= (26, 1) => Some(25),
        [1, minor] | [1, minor, _] => {
            let patch = numbers.get(2).copied().unwrap_or(0);
            Some(if (*minor, patch) >= (20, 5) {
                21
            } else if *minor >= 18 {
                17
            } else if *minor >= 17 {
                16
            } else {
                8
            })
        }
        _ => None,
    }
}
fn release_date_java(timestamp: &str) -> Option<u64> {
    let date = timestamp.get(..10)?;
    // Date fallback only accepts the UTC ISO form used by Mojang metadata.
    let time = timestamp.get(10..)?.strip_prefix('T')?;
    let time = time
        .strip_suffix('Z')
        .or_else(|| time.strip_suffix("+00:00"))?;
    let time = if let Some((time, fraction)) = time.split_once('.') {
        if fraction.is_empty() || !fraction.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        time
    } else {
        time
    };
    if date.as_bytes().get(4) != Some(&b'-')
        || date.as_bytes().get(7) != Some(&b'-')
        || time.len() != 8
        || time.as_bytes()[2] != b':'
        || time.as_bytes()[5] != b':'
    {
        return None;
    }
    if !date.bytes().enumerate().all(|(i, c)| {
        if i == 4 || i == 7 {
            c == b'-'
        } else {
            c.is_ascii_digit()
        }
    }) || !time.bytes().enumerate().all(|(i, c)| {
        if i == 2 || i == 5 {
            c == b':'
        } else {
            c.is_ascii_digit()
        }
    }) {
        return None;
    }
    let parts: Vec<u32> = time
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if parts.len() != 3 || parts[0] > 23 || parts[1] > 59 || parts[2] > 59 {
        return None;
    }
    let parts: Vec<u32> = date
        .split('-')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let [year, month, day]: [u32; 3] = parts.try_into().ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if year < 2001 || day == 0 || day > days {
        return None;
    }
    Some(if date >= "2024-04-02" {
        21
    } else if date >= "2021-11-16" {
        17
    } else if date >= "2021-05-11" {
        16
    } else {
        8
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::Cell, io::Write};

    struct Source {
        calls: Cell<usize>,
        failure: bool,
        info: fn() -> ClientJavaInfo,
    }
    impl ClientVersionSource for Source {
        fn read(&self, _: &Path, _: &TaskControl) -> Result<ClientJavaInfo> {
            self.calls.set(self.calls.get() + 1);
            if self.failure {
                bail!("archive unavailable");
            }
            Ok((self.info)())
        }
    }
    fn stub(info: fn() -> ClientJavaInfo) -> Source {
        Source {
            calls: Cell::new(0),
            failure: false,
            info,
        }
    }
    fn version(base: &str, meta: Value) -> ResolvedVersion {
        ResolvedVersion {
            meta,
            jar_id: "client-owner".into(),
            chain: vec![base.into(), "renamed-instance".into()],
        }
    }
    fn resolve(source: &dyn ClientVersionSource, base: &str, meta: Value) -> Result<GameJavaPlan> {
        GameJavaResolver::new(source).resolve(
            Path::new("unused"),
            &version(base, meta),
            &TaskControl::default(),
        )
    }
    #[test]
    fn cleanroom_plans_replace_inherited_java_component_and_preserve_higher_requirements() {
        let source = stub(ClientJavaInfo::default);
        for (loader, expected) in [
            ("0.4.9", 21),
            ("0.5.0-alpha", 25),
            ("0.5.0", 25),
            ("1.0.0", 25),
        ] {
            for explicit in [false, true] {
                let mut meta =
                    json!({"libraries":[{"name":format!("com.cleanroommc:cleanroom:{loader}")}]});
                if explicit {
                    meta["javaVersion"] = json!({"majorVersion":8,"component":"jre-legacy"});
                }
                let plan = resolve(&source, "1.12.2", meta).unwrap();
                assert_eq!(plan.requirement.recommended(), expected);
                assert!(plan.component.is_none());
                assert_eq!(plan.origin, "加载器 Java 要求");
            }
        }
        let plan = resolve(
            &source,
            "1.12.2",
            json!({
                "libraries":[{"name":"com.cleanroommc:cleanroom:0.4.9"}],
                "javaVersion":{"majorVersion":25,"component":"java-runtime-epsilon"}
            }),
        )
        .unwrap();
        assert_eq!(plan.requirement.recommended(), 25);
        assert_eq!(plan.component.as_deref(), Some("java-runtime-epsilon"));
    }
    #[test]
    fn explicit_requirement_wins_without_accessing_client() {
        let mut source = stub(ClientJavaInfo::default);
        source.failure = true;
        let plan = resolve(
            &source,
            "26.1",
            json!({"javaVersion":{"majorVersion":17,"component":"java-runtime-gamma"}}),
        )
        .unwrap();
        assert_eq!(plan.requirement.recommended(), 17);
        assert_eq!(source.calls.get(), 0);
        for bad in [
            json!("21"),
            json!({"majorVersion":0}),
            json!({"majorVersion":null}),
            json!({"component":"../escape"}),
        ] {
            assert!(resolve(&source, "1.21.1", json!({"javaVersion":bad})).is_err());
        }
        assert_eq!(source.calls.get(), 0);
    }
    #[test]
    fn embedded_java_and_component_survive_renamed_profiles() {
        let source = stub(|| ClientJavaInfo {
            java: Some(25),
            component: Some("java-runtime-epsilon".into()),
            ..Default::default()
        });
        let plan = resolve(&source, "totally-renamed", json!({"id":"misleading-1.8"})).unwrap();
        assert_eq!(plan.requirement.recommended(), 25);
        assert_eq!(plan.component.as_deref(), Some("java-runtime-epsilon"));
        let plan = resolve(
            &source,
            "unknown",
            json!({"javaVersion":{"component":"custom-jre"}}),
        )
        .unwrap();
        assert_eq!(plan.component.as_deref(), Some("custom-jre"));
    }
    #[test]
    fn vanilla_base_fallback_ignores_child_labels_and_respects_boundaries() {
        let source = stub(ClientJavaInfo::default);
        for (base, expected) in [
            ("1.16.5", 8),
            ("1.17", 16),
            ("1.17.1", 16),
            ("1.18", 17),
            ("1.20.4", 17),
            ("1.20.5", 21),
            ("1.21.1", 21),
            ("26.1", 25),
        ] {
            assert_eq!(
                resolve(&source, base, json!({"id":"1.8"}))
                    .unwrap()
                    .requirement
                    .recommended(),
                expected,
                "{base}"
            );
        }
        assert_eq!(
            resolve(&source, "unknown", json!({"id":"1.21.1"}))
                .unwrap()
                .requirement
                .recommended(),
            8
        );
        assert_eq!(release_java("1.20.5-custom"), None);
        assert_eq!(release_java("my-1.21.1-pack"), None);
    }
    #[test]
    fn bytecode_floor_raises_an_embedded_or_fallback_requirement() {
        let source = stub(|| ClientJavaInfo {
            java: Some(17),
            class_java: Some(21),
            ..Default::default()
        });
        let plan = resolve(&source, "1.18", json!({})).unwrap();
        assert_eq!(plan.requirement.recommended(), 21);
        assert_eq!(plan.origin, "客户端字节码版本");
        let source = stub(|| ClientJavaInfo {
            version: Some("1.20.4".into()),
            ..Default::default()
        });
        assert_eq!(
            resolve(&source, "1.21.1", json!({}))
                .unwrap()
                .requirement
                .recommended(),
            17
        );
    }
    #[test]
    fn snapshot_date_fallback_uses_precise_pcl_cutoffs() {
        for (date, expected) in [
            ("2021-05-10", 8),
            ("2021-05-11", 16),
            ("2021-11-15", 16),
            ("2021-11-16", 17),
            ("2024-04-01", 17),
            ("2024-04-02", 21),
        ] {
            assert_eq!(
                release_date_java(&format!("{date}T12:00:00Z")),
                Some(expected)
            );
            assert_eq!(
                release_date_java(&format!("{date}T12:00:00.123+00:00")),
                Some(expected)
            );
        }
        for date in [
            "2024-02-30T00:00:00Z",
            "2024-04-02",
            "1960-01-01T00:00:00Z",
            "2024-04-02T88:00:00Z",
            "2024-04-02T12:00:00.badZ",
            "2024-4-002T12:00:00Z",
        ] {
            assert_eq!(release_date_java(date), None, "{date}");
        }
        assert_eq!(
            resolve(
                &stub(ClientJavaInfo::default),
                "snapshot",
                json!({"releaseTime":"2024-04-02T00:00:00Z"})
            )
            .unwrap()
            .requirement
            .recommended(),
            21
        );
    }
    #[test]
    fn errors_and_cancellation_do_not_turn_into_legacy_defaults() {
        let mut source = stub(ClientJavaInfo::default);
        source.failure = true;
        assert!(resolve(&source, "1.21.1", json!({})).is_err());
        let control = TaskControl::default();
        control.cancel();
        assert!(GameJavaResolver::new(&source)
            .resolve(Path::new("unused"), &version("1.21.1", json!({})), &control)
            .is_err());
        assert_eq!(source.calls.get(), 1);
        struct Cancelling;
        impl ClientVersionSource for Cancelling {
            fn read(&self, _: &Path, control: &TaskControl) -> Result<ClientJavaInfo> {
                control.cancel();
                Ok(ClientJavaInfo::default())
            }
        }
        let error = resolve(&Cancelling, "1.21.1", json!({})).err().unwrap();
        assert!(format!("{error:#}").contains("取消"));
    }
    #[test]
    fn component_major_conflicts_fail_before_runtime_download() {
        let source = stub(|| ClientJavaInfo {
            java: Some(25),
            component: Some("java-runtime-delta".into()),
            ..Default::default()
        });
        assert!(resolve(&source, "26.1", json!({})).is_err());
        assert!(resolve(
            &source,
            "1.21.1",
            json!({"javaVersion":{"majorVersion":21,"component":"jre-legacy"}})
        )
        .is_err());
        assert!(resolve(
            &source,
            "1.21.1",
            json!({"javaVersion":{"majorVersion":21,"component":"custom-jre"}})
        )
        .is_ok());
    }
    #[test]
    fn known_component_supplies_missing_major_without_using_package_date() {
        let source = stub(ClientJavaInfo::default);
        for (component, expected) in [
            ("jre-legacy", 8),
            ("java-runtime-beta", 17),
            ("java-runtime-epsilon", 25),
        ] {
            let plan = resolve(
                &source,
                "unknown",
                json!({"javaVersion":{"component":component},"releaseTime":"2026-01-01T00:00:00Z"}),
            )
            .unwrap();
            assert_eq!(plan.requirement.recommended(), expected);
        }
        let plan = resolve(
            &source,
            "1.20.5",
            json!({"javaVersion":{"component":"java-runtime-epsilon"}}),
        )
        .unwrap();
        assert_eq!(plan.requirement.recommended(), 25);
        assert!(resolve(
            &source,
            "26.1",
            json!({"javaVersion":{"component":"jre-legacy"}})
        )
        .is_err());
    }
    #[test]
    fn resolved_profile_uses_actual_inherited_or_replaced_client_jar() {
        let workspace = crate::test_support::Workspace::new();
        for id in ["1.20.5", "my-pack"] {
            fs::create_dir_all(workspace.path.join("versions").join(id)).unwrap();
        }
        let base = workspace.path.join("versions/1.20.5");
        let child = workspace.path.join("versions/my-pack");
        fs::write(
            base.join("1.20.5.json"),
            br#"{"id":"1.20.5","mainClass":"net.minecraft.client.main.Main","libraries":[]}"#,
        )
        .unwrap();
        fs::write(
            child.join("my-pack.json"),
            br#"{"id":"my-pack","inheritsFrom":"1.20.5"}"#,
        )
        .unwrap();
        write_jar(
            &base.join("1.20.5.jar"),
            Some(br#"{"id":"1.20.5","java_version":21}"#),
            Some(65),
        );
        let resolver = GameJavaResolver::new(&JarClientVersionSource);
        let resolved = crate::version::resolve(&workspace.path, "my-pack").unwrap();
        assert_eq!(resolved.jar_id, "1.20.5");
        assert_eq!(
            resolver
                .resolve(&workspace.path, &resolved, &TaskControl::default())
                .unwrap()
                .requirement
                .recommended(),
            21
        );
        fs::write(
            child.join("my-pack.json"),
            br#"{"id":"my-pack","inheritsFrom":"1.20.5","downloads":{"client":{}}}"#,
        )
        .unwrap();
        write_jar(
            &child.join("my-pack.jar"),
            Some(br#"{"id":"26.1","java_version":25}"#),
            Some(69),
        );
        let resolved = crate::version::resolve(&workspace.path, "my-pack").unwrap();
        assert_eq!(resolved.jar_id, "my-pack");
        assert_eq!(
            resolver
                .resolve(&workspace.path, &resolved, &TaskControl::default())
                .unwrap()
                .requirement
                .recommended(),
            25
        );
    }
    fn write_jar(path: &Path, metadata: Option<&[u8]>, class_major: Option<u16>) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        if let Some(metadata) = metadata {
            zip.start_file("version.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(metadata).unwrap();
        }
        if let Some(major) = class_major {
            zip.start_file(
                "net/minecraft/client/main/Main.class",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(&[
                0xca,
                0xfe,
                0xba,
                0xbe,
                0,
                0,
                (major >> 8) as u8,
                major as u8,
            ])
            .unwrap();
        }
        zip.finish().unwrap();
    }
    #[test]
    fn client_identity_rejects_hidden_duplicate_metadata_and_entry_points() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("ambiguous-client.jar");
        for (original, alias, first, second) in [
            (
                "version.json",
                "versiom.json",
                br#"{"java_version":17}"#.as_slice(),
                br#"{"java_version":21}"#.as_slice(),
            ),
            (
                "net/minecraft/client/main/Main.class",
                "net/minecraft/client/main/Maim.class",
                b"\xca\xfe\xba\xbe\0\0\0\x3d".as_slice(),
                b"\xca\xfe\xba\xbe\0\0\0\x41".as_slice(),
            ),
        ] {
            let mut archive = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            for (name, content) in [(original, first), (alias, second)] {
                archive
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                archive.write_all(content).unwrap();
            }
            archive.finish().unwrap();
            let mut bytes = fs::read(&path).unwrap();
            for offset in 0..=bytes.len() - alias.len() {
                if &bytes[offset..offset + alias.len()] == alias.as_bytes() {
                    bytes[offset..offset + alias.len()].copy_from_slice(original.as_bytes());
                }
            }
            fs::write(&path, bytes).unwrap();
            let error = JarClientVersionSource
                .read(&path, &TaskControl::default())
                .err()
                .expect("ambiguous client must be rejected");
            let message = format!("{error:#}");
            assert!(message.contains("重复"), "{message}");
            assert!(message.contains("ambiguous-client.jar"), "{message}");
        }
    }
    #[test]
    fn real_zip_provider_reads_embedded_metadata_and_bytecode_header() {
        let workspace = crate::test_support::Workspace::new();
        let jar = workspace.path.join("client.jar");
        write_jar(&jar,Some(br#"{"id":"26.1 / custom","java_version":25,"java_component":"java-runtime-epsilon"}"#),Some(69));
        let info = JarClientVersionSource
            .read(&jar, &TaskControl::default())
            .unwrap();
        assert_eq!(info.version.as_deref(), Some("26.1"));
        assert_eq!(info.java, Some(25));
        assert_eq!(info.class_java, Some(25));
        assert_eq!(info.component.as_deref(), Some("java-runtime-epsilon"));
        write_jar(&jar, None, Some(60));
        let info = JarClientVersionSource
            .read(&jar, &TaskControl::default())
            .unwrap();
        assert_eq!(info.class_java, Some(16));
        assert!(info.java.is_none());
    }
    #[test]
    fn real_zip_provider_rejects_bad_or_oversized_metadata_and_bad_class() {
        let workspace = crate::test_support::Workspace::new();
        let jar = workspace.path.join("client.jar");
        for metadata in [
            b"{".as_slice(),
            br#"{"java_version":0}"#,
            br#"{"java_version":"21"}"#,
            b"[]",
        ] {
            write_jar(&jar, Some(metadata), None);
            assert!(JarClientVersionSource
                .read(&jar, &TaskControl::default())
                .is_err());
        }
        write_jar(&jar, Some(&vec![b' '; 1024 * 1024 + 1]), None);
        assert!(JarClientVersionSource
            .read(&jar, &TaskControl::default())
            .is_err());
        write_jar(&jar, None, Some(1));
        assert!(JarClientVersionSource
            .read(&jar, &TaskControl::default())
            .is_err());
        assert!(JarClientVersionSource
            .read(&workspace.path.join("missing.jar"), &TaskControl::default())
            .unwrap()
            .java
            .is_none());
    }
}
