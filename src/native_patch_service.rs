use crate::{java_architecture::JavaArchitecture, metadata::os_name, task::TaskControl};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeArtifact {
    url: String,
    sha1: String,
    size: u64,
    extracted: bool,
}
pub(crate) enum NativeReplacement {
    Keep,
    Remove,
    Artifact(String, NativeArtifact),
}
/// Pinned artifact facts are replaceable without giving the policy network access.
pub(crate) trait NativePatchCatalog {
    fn replacement(&self, platform: &str, source: &str) -> Result<NativeReplacement>;
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema: u32,
    targets: HashMap<String, HashMap<String, Option<String>>>,
    artifacts: HashMap<String, NativeArtifact>,
}
impl Catalog {
    fn parse(source: &str) -> Result<Self> {
        let catalog: Self = serde_json::from_str(source)?;
        if catalog.schema != 1 {
            bail!("未知 native 补丁目录格式");
        }
        for (target, replacements) in &catalog.targets {
            if !matches!(
                target.as_str(),
                "linux-aarch64" | "osx-aarch64" | "windows-aarch64"
            ) {
                bail!("未知 native 补丁目标：{target}");
            }
            for (source, artifact) in replacements {
                crate::version::maven_path(source)?;
                if let Some(artifact) = artifact {
                    catalog
                        .artifacts
                        .get(artifact)
                        .with_context(|| format!("native 补丁缺少下载信息：{artifact}"))?;
                }
            }
        }
        for (name, artifact) in &catalog.artifacts {
            artifact.library(name, "linux", &Value::Null)?;
        }
        Ok(catalog)
    }
}
pub(crate) struct EmbeddedNativeCatalog;
impl NativePatchCatalog for EmbeddedNativeCatalog {
    fn replacement(&self, platform: &str, source: &str) -> Result<NativeReplacement> {
        static CATALOG: OnceLock<std::result::Result<Catalog, String>> = OnceLock::new();
        let catalog = CATALOG
            .get_or_init(|| {
                Catalog::parse(include_str!("../assets/native-patches.json"))
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map_err(|error| anyhow::anyhow!("内置 native 补丁目录无效：{error}"))?;
        Ok(
            match catalog
                .targets
                .get(platform)
                .and_then(|target| target.get(source))
            {
                None => NativeReplacement::Keep,
                Some(None) => NativeReplacement::Remove,
                Some(Some(name)) => {
                    NativeReplacement::Artifact(name.clone(), catalog.artifacts[name].clone())
                }
            },
        )
    }
}
impl NativeArtifact {
    fn library(&self, name: &str, os: &str, original: &Value) -> Result<Value> {
        let path = crate::version::maven_path(name)?;
        crate::metadata_cache::validate_url(&self.url)?;
        if self.sha1.len() != 40
            || !self.sha1.bytes().all(|c| c.is_ascii_hexdigit())
            || self.size == 0
        {
            bail!("native 补丁必须有有效 SHA-1 与大小：{name}");
        }
        let download = json!({"path":path,"url":self.url,"sha1":self.sha1,"size":self.size});
        if self.extracted {
            let parts: Vec<_> = name.split(':').collect();
            if parts.len() != 4 || parts[3].is_empty() {
                bail!("需解压的 native 补丁缺少分类：{name}");
            }
            Ok(json!({"name":parts[..3].join(":"),
                "downloads":{"classifiers":{parts[3]:download}},
                "natives":{os:parts[3]},
                "extract":original.get("extract").cloned().unwrap_or_else(|| json!({"exclude":["META-INF/"]}))}))
        } else {
            Ok(json!({"name":name,"downloads":{"artifact":download}}))
        }
    }
}

pub(crate) struct NativePatchPlan {
    pub(crate) profile: Value,
    pub(crate) messages: Vec<String>,
    pub(crate) lwjgl_changed: bool,
}
pub(crate) struct NativePatchService<'a> {
    os: &'a str,
    architecture: JavaArchitecture,
    catalog: &'a dyn NativePatchCatalog,
}
impl<'a> NativePatchService<'a> {
    pub(crate) fn new(
        os: &'a str,
        architecture: JavaArchitecture,
        catalog: &'a dyn NativePatchCatalog,
    ) -> Self {
        Self {
            os,
            architecture,
            catalog,
        }
    }
    fn official_arm(&self, chain: &[String]) -> bool {
        if !matches!(self.os, "windows" | "osx") {
            return false;
        }
        chain
            .first()
            .and_then(|id| {
                let numbers = id
                    .split('.')
                    .map(str::parse::<u32>)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .ok()?;
                match numbers.as_slice() {
                    [1, minor] | [1, minor, _] => Some(*minor >= 19),
                    [major, _] | [major, _, _] if *major >= 26 => Some(true),
                    _ => None,
                }
            })
            .unwrap_or(false)
    }
    pub(crate) fn patch(
        &self,
        profile: &Value,
        chain: &[String],
        enabled: bool,
        control: &TaskControl,
    ) -> Result<NativePatchPlan> {
        control.check()?;
        let mut result = NativePatchPlan {
            profile: profile.clone(),
            messages: vec![],
            lwjgl_changed: false,
        };
        if !enabled || self.architecture != JavaArchitecture::Arm64 || self.official_arm(chain) {
            return Ok(result);
        }
        let libraries = profile["libraries"]
            .as_array()
            .context("native 补丁缺少 libraries")?;
        let arm_families: HashSet<_> = libraries
            .iter()
            .filter(|library| {
                library["natives"][self.os]
                    .as_str()
                    .into_iter()
                    .chain(
                        library["name"]
                            .as_str()
                            .and_then(|name| name.split(':').nth(3)),
                    )
                    .any(|classifier| {
                        classifier.contains("arm64") || classifier.contains("aarch64")
                    })
            })
            .filter_map(|library| library["name"].as_str())
            .map(|name| name.split(':').take(3).collect::<Vec<_>>().join(":"))
            .collect();
        let mut output = vec![];
        for library in libraries {
            control.check()?;
            let Some(name) = library["name"].as_str() else {
                output.push(library.clone());
                continue;
            };
            if !name.starts_with("org.lwjgl:")
                && !name.starts_with("org.lwjgl.lwjgl:")
                && !name.starts_with("net.java.jinput:")
                && !name.starts_with("com.mojang:text2speech:")
            {
                output.push(library.clone());
                continue;
            }
            // A combined library may carry both an ordinary artifact and an extracted
            // classifier. Patch these independently so removing speech/input natives
            // cannot accidentally delete their Java APIs.
            let classifier = library["natives"][self.os].as_str();
            if arm_families.contains(&name.split(':').take(3).collect::<Vec<_>>().join(":")) {
                output.push(library.clone());
                continue;
            }
            let mut parts = vec![];
            if classifier.is_some() {
                if library
                    .pointer("/downloads/artifact")
                    .is_some_and(|value| !value.is_null())
                {
                    let mut main = library.clone();
                    let object = main.as_object_mut().context("依赖库必须为对象")?;
                    object.remove("natives");
                    object.remove("extract");
                    if let Some(downloads) = main["downloads"].as_object_mut() {
                        downloads.remove("classifiers");
                    }
                    parts.push((name.to_owned(), main));
                }
                let mut native = library.clone();
                if let Some(downloads) = native["downloads"].as_object_mut() {
                    downloads.remove("artifact");
                }
                parts.push((format!("{name}:natives"), native));
            } else {
                parts.push((name.to_owned(), library.clone()));
            }
            let mut changed = false;
            let mut patched = vec![];
            for (key, original) in parts {
                match self.catalog.replacement(
                    &format!("{}-{}", self.os, self.architecture.rule_name()),
                    &key,
                )? {
                    NativeReplacement::Keep => {
                        if key.ends_with(":natives") || key.contains(":natives-") {
                            result
                                .messages
                                .push(format!("没有已知 ARM native 替换方案，保留：{key}"));
                        }
                        patched.push(original);
                    }
                    NativeReplacement::Remove => {
                        changed = true;
                        result
                            .messages
                            .push(format!("此平台不提供该原生组件，已从启动依赖禁用：{key}"));
                    }
                    NativeReplacement::Artifact(name, artifact) => {
                        let replacement = artifact.library(&name, self.os, &original)?;
                        if key.starts_with("org.lwjgl:")
                            && !key.contains(":natives")
                            && key.split(':').nth(2) != name.split(':').nth(2)
                        {
                            result.lwjgl_changed = true;
                        }
                        result
                            .messages
                            .push(format!("ARM 兼容依赖：{key} → {name}"));
                        patched.push(replacement);
                        changed = true;
                    }
                }
            }
            if changed {
                output.extend(patched);
            } else {
                output.push(library.clone());
            }
        }
        control.check()?;
        result.profile["libraries"] = Value::Array(output);
        Ok(result)
    }
}

pub(crate) fn current_service(architecture: JavaArchitecture) -> NativePatchService<'static> {
    NativePatchService::new(os_name(), architecture, &EmbeddedNativeCatalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_service::LibraryResolver;
    fn combined(name: &str, os: &str) -> Value {
        json!({"name":name,"downloads":{
            "artifact":{"url":"https://fixture.invalid/main.jar"},
            "classifiers":{"native-old":{"url":"https://fixture.invalid/old.jar"}}},
            "natives":{os:"native-old"}})
    }
    fn patch(os: &str, profile: &Value) -> NativePatchPlan {
        NativePatchService::new(os, JavaArchitecture::Arm64, &EmbeddedNativeCatalog)
            .patch(profile, &["renamed".into()], true, &TaskControl::default())
            .unwrap()
    }
    #[test]
    fn embedded_catalog_validates_every_pinned_artifact_and_mapping() {
        let catalog = Catalog::parse(include_str!("../assets/native-patches.json")).unwrap();
        assert_eq!(catalog.artifacts.len(), 83);
        assert_eq!(
            catalog.targets.values().map(HashMap::len).sum::<usize>(),
            187
        );
        for (target, mappings) in &catalog.targets {
            let os = target.split('-').next().unwrap();
            for name in mappings.values().flatten() {
                let artifact = &catalog.artifacts[name];
                let library = artifact.library(name, os, &Value::Null).unwrap();
                let resolved = LibraryResolver::new(os, "64").resolve(&library).unwrap();
                assert_eq!(resolved.len(), 1, "{target} {name}");
                assert_eq!(resolved[0].1, artifact.extracted);
                assert_eq!(resolved[0].0["sha1"], artifact.sha1);
                assert_eq!(resolved[0].0["size"], artifact.size);
            }
        }
    }
    #[test]
    fn legacy_lwjgl3_updates_api_and_native_together_on_all_three_arm_targets() {
        for (os, version, classifier) in [
            ("linux", "3.3.2", "natives-linux-arm64"),
            ("windows", "3.3.2", "natives-windows-arm64"),
            ("osx", "3.3.1", "natives-macos-arm64"),
        ] {
            let profile = json!({"libraries":[combined("org.lwjgl:lwjgl:3.1.6",os)]});
            let plan = patch(os, &profile);
            let libraries = plan.profile["libraries"].as_array().unwrap();
            assert_eq!(libraries.len(), 2);
            assert_eq!(libraries[0]["name"], format!("org.lwjgl:lwjgl:{version}"));
            assert_eq!(
                libraries[1]["name"],
                format!("org.lwjgl:lwjgl:{version}:{classifier}")
            );
            assert!(plan.lwjgl_changed);
            assert_eq!(profile["libraries"][0]["name"], "org.lwjgl:lwjgl:3.1.6");
            for library in libraries {
                assert!(!LibraryResolver::new(os, "64").resolve(library).unwrap()[0].1);
            }
            assert_eq!(patch(os, &plan.profile).profile, plan.profile);
            assert!(patch(os, &plan.profile).messages.is_empty());
        }
        let mac = patch(
            "osx",
            &json!({"libraries":[combined("org.lwjgl:lwjgl-glfw:3.2.1","osx")]}),
        );
        assert_eq!(
            mac.profile["libraries"][0]["name"],
            "org.glavo.hmcl.mmachina:lwjgl-glfw:3.3.1-mmachina.1"
        );
    }
    #[test]
    fn lwjgl2_replacement_stays_an_extracted_native_without_a_phantom_main_jar() {
        for os in ["linux", "windows", "osx"] {
            let profile = json!({"libraries":[{"name":"org.lwjgl.lwjgl:lwjgl-platform:2.9.1",
                "natives":{os:"old"},"extract":{"exclude":["META-INF/","custom/"]}}]});
            let plan = patch(os, &profile);
            let library = &plan.profile["libraries"][0];
            let entries = LibraryResolver::new(os, "64").resolve(library).unwrap();
            assert_eq!(entries.len(), 1);
            assert!(entries[0].1);
            assert!(entries[0].0["path"]
                .as_str()
                .unwrap()
                .contains("lwjgl2-natives"));
            assert_eq!(library["extract"]["exclude"][1], "custom/");
            assert!(!plan.lwjgl_changed);
        }
    }
    #[test]
    fn removing_unsupported_speech_or_controller_natives_preserves_their_java_api() {
        for os in ["linux", "windows", "osx"] {
            let profile = json!({"libraries":[combined("com.mojang:text2speech:1.10.3",os),
                combined("net.java.jinput:jinput-platform:2.0.5",os)]});
            let plan = patch(os, &profile);
            let libraries = plan.profile["libraries"].as_array().unwrap();
            assert_eq!(libraries.len(), 2);
            assert!(libraries.iter().all(|lib| lib.get("natives").is_none()));
            let speech = if os == "osx" {
                "com.mojang:text2speech:1.11.3"
            } else {
                "com.mojang:text2speech:1.10.3"
            };
            assert_eq!(libraries[0]["name"], speech);
            assert!(plan.messages.iter().any(|message| message.contains("禁用")));
        }
    }
    #[test]
    fn modern_linux_changes_only_native_classifier_and_official_windows_macos_are_preserved() {
        let profile = json!({"libraries":[{"name":"org.lwjgl:lwjgl:3.3.3:natives-linux",
            "downloads":{"artifact":{"url":"https://fixture.invalid/native.jar"}}}]});
        let plan = patch("linux", &profile);
        assert_eq!(
            plan.profile["libraries"][0]["name"],
            "org.lwjgl:lwjgl:3.3.3:natives-linux-arm64"
        );
        assert!(!plan.lwjgl_changed);
        for os in ["windows", "osx"] {
            let input = json!({"libraries":[combined("com.mojang:text2speech:1.12.4",os)]});
            for base in ["1.19", "1.20.1", "26.1"] {
                let plan =
                    NativePatchService::new(os, JavaArchitecture::Arm64, &EmbeddedNativeCatalog)
                        .patch(
                            &input,
                            &[base.into(), "renamed".into()],
                            true,
                            &TaskControl::default(),
                        )
                        .unwrap();
                assert_eq!(plan.profile, input);
                assert!(plan.messages.is_empty());
            }
        }
    }
    #[test]
    fn disabled_patching_other_architectures_unknown_and_custom_arm_libraries_are_preserved() {
        let original = json!({"libraries":[combined("org.lwjgl:lwjgl:3.1.6","linux")]});
        for (architecture, enabled) in [
            (JavaArchitecture::X64, true),
            (JavaArchitecture::Arm64, false),
        ] {
            assert_eq!(
                NativePatchService::new("linux", architecture, &EmbeddedNativeCatalog)
                    .patch(&original, &[], enabled, &TaskControl::default())
                    .unwrap()
                    .profile,
                original
            );
        }
        let unknown = json!({"libraries":[combined("org.lwjgl:lwjgl:9.0.0","linux"),
            {"name":"custom:binding:1","url":"https://custom.invalid/"}]});
        let plan = patch("linux", &unknown);
        assert_eq!(plan.profile, unknown);
        assert_eq!(plan.messages.len(), 1);
        let mut custom = original.clone();
        custom["libraries"][0]["natives"]["linux"] = "custom-arm64".into();
        assert_eq!(patch("linux", &custom).profile, custom);
        let separated = json!({"libraries":[
            {"name":"org.lwjgl:lwjgl:3.1.6","url":"https://custom.invalid/"},
            {"name":"org.lwjgl:lwjgl:3.1.6:natives-linux-arm64","url":"https://custom.invalid/"}
        ]});
        assert_eq!(patch("linux", &separated).profile, separated);
        assert!(patch("linux", &separated).messages.is_empty());
    }
    #[test]
    fn an_editable_child_label_does_not_override_the_base_version_gate() {
        let source = json!({"libraries":[combined("org.lwjgl:lwjgl:3.1.6","osx")]});
        let service =
            NativePatchService::new("osx", JavaArchitecture::Arm64, &EmbeddedNativeCatalog);
        let result = service
            .patch(
                &source,
                &["custom-base".into(), "1.20.1".into()],
                true,
                &TaskControl::default(),
            )
            .unwrap();
        assert!(result.lwjgl_changed);
        assert_ne!(result.profile, source);
    }
    struct Failure;
    impl NativePatchCatalog for Failure {
        fn replacement(&self, _: &str, _: &str) -> Result<NativeReplacement> {
            bail!("catalog unavailable")
        }
    }
    #[test]
    fn catalog_failure_or_cancellation_does_not_produce_a_partial_launch_plan() {
        let profile = json!({"libraries":[combined("org.lwjgl:lwjgl:3.1.6","linux")]});
        let service = NativePatchService::new("linux", JavaArchitecture::Arm64, &Failure);
        let control = TaskControl::default();
        let error = service.patch(&profile, &[], true, &control).err().unwrap();
        assert!(error.to_string().contains("catalog unavailable"));
        control.cancel();
        assert!(service
            .patch(&profile, &[], true, &control)
            .err()
            .unwrap()
            .to_string()
            .contains("取消"));
        assert_eq!(profile["libraries"][0]["name"], "org.lwjgl:lwjgl:3.1.6");
    }
    #[test]
    fn invalid_catalog_versions_missing_artifacts_and_unsafe_downloads_are_rejected() {
        let original: Value =
            serde_json::from_str(include_str!("../assets/native-patches.json")).unwrap();
        let mut bad = original.clone();
        bad["schema"] = 99.into();
        assert!(Catalog::parse(&bad.to_string()).is_err());
        let mut bad = original.clone();
        bad["artifacts"] = json!({});
        assert!(Catalog::parse(&bad.to_string()).is_err());
        for field in ["url", "sha1", "size"] {
            let mut bad = original.clone();
            let artifact = bad["artifacts"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap();
            artifact[field] = match field {
                "url" => json!("http://unsafe.invalid/a.jar"),
                "sha1" => json!("bad"),
                _ => json!(0),
            };
            assert!(Catalog::parse(&bad.to_string()).is_err(), "{field}");
        }
    }
}
