use crate::{
    metadata::{os_name, safe_path},
    version::maven_path,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Resolves a library for one platform without touching the network or filesystem.
pub struct LibraryResolver<'a> {
    os: &'a str,
    arch: &'a str,
}
impl<'a> LibraryResolver<'a> {
    pub fn new(os: &'a str, arch: &'a str) -> Self {
        Self { os, arch }
    }
    pub fn current() -> LibraryResolver<'static> {
        LibraryResolver::new(
            os_name(),
            if cfg!(target_pointer_width = "64") {
                "64"
            } else {
                "32"
            },
        )
    }
    pub fn resolve(&self, library: &Value) -> Result<Vec<(Value, bool)>> {
        if !library.is_object() {
            bail!("依赖库必须为 JSON 对象");
        }
        let mut entries = vec![];
        if let Some(artifact) = library.pointer("/downloads/artifact") {
            entries.push((self.complete(artifact, library, None)?, false));
        } else if library.get("downloads").is_none() {
            entries.push((self.legacy(library, None)?, false));
        }
        if let Some(classifier) = library["natives"][self.os].as_str() {
            let classifier = classifier.replace("${arch}", self.arch);
            let entry = match library
                .pointer("/downloads/classifiers")
                .and_then(|items| items.get(&classifier))
            {
                Some(entry) => self.complete(entry, library, Some(&classifier))?,
                None if library.get("downloads").is_none() => {
                    self.legacy(library, Some(&classifier))?
                }
                None => bail!("当前平台缺少 native 分类：{classifier}"),
            };
            entries.push((entry, true));
        }
        Ok(entries)
    }
    fn path(&self, library: &Value, classifier: Option<&str>) -> Result<String> {
        let name = library["name"].as_str().context("库缺少 Maven 坐标")?;
        let Some(classifier) = classifier else {
            return maven_path(name);
        };
        let (coordinate, extension) = name.split_once('@').unwrap_or((name, "jar"));
        let parts: Vec<_> = coordinate.split(':').collect();
        if !(3..=4).contains(&parts.len()) {
            bail!("无效 Maven 坐标：{name}");
        }
        maven_path(&format!(
            "{}:{}:{}:{classifier}@{extension}",
            parts[0], parts[1], parts[2]
        ))
    }
    fn legacy(&self, library: &Value, classifier: Option<&str>) -> Result<Value> {
        let path = self.path(library, classifier)?;
        let base = library["url"]
            .as_str()
            .unwrap_or("https://libraries.minecraft.net/");
        let mut repository = reqwest::Url::parse(base).context("无效 Maven 仓库地址")?;
        if repository.scheme() == "http" && repository.host_str() == Some("libraries.minecraft.net")
        {
            repository
                .set_scheme("https")
                .map_err(|_| anyhow::anyhow!("无法升级 Maven 仓库为 HTTPS"))?;
        }
        if repository.scheme() != "https"
            || repository.query().is_some()
            || repository.fragment().is_some()
        {
            bail!("Maven 仓库必须使用不含查询与片段的 HTTPS 地址");
        }
        let mut entry = json!({"path":path, "url":format!("{}/{}", repository.as_str().trim_end_matches('/'), path)});
        // A checksum on a legacy library describes the main JAR, not its native classifier.
        if classifier.is_none() {
            for key in ["sha1", "size"] {
                if let Some(value) = library.get(key) {
                    entry[key] = value.clone();
                }
            }
        }
        Ok(entry)
    }
    fn complete(
        &self,
        artifact: &Value,
        library: &Value,
        classifier: Option<&str>,
    ) -> Result<Value> {
        if !artifact.is_object() {
            bail!("依赖下载项必须为 JSON 对象");
        }
        let mut entry = artifact.clone();
        let path = match entry.get("path") {
            Some(Value::String(path)) => path.clone(),
            None => self.path(library, classifier)?,
            _ => bail!("依赖路径必须为字符串"),
        };
        safe_path(Path::new("libraries"), &path)?;
        entry["path"] = json!(path);
        if entry["url"].as_str().is_none_or(str::is_empty) {
            bail!("资源缺少下载地址");
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_natives_use_the_classifier_and_do_not_reuse_main_jar_checksums() {
        let library = json!({"name":"org.example:binding:1.2", "url":"https://repo.example/maven/",
            "sha1":"main-hash", "size":42, "natives":{"windows":"natives-windows-${arch}"}});
        let resolved = LibraryResolver::new("windows", "64")
            .resolve(&library)
            .unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(!resolved[0].1);
        assert_eq!(resolved[0].0["sha1"], "main-hash");
        assert!(resolved[1].1);
        assert_eq!(
            resolved[1].0["path"],
            "org/example/binding/1.2/binding-1.2-natives-windows-64.jar"
        );
        assert_eq!(
            resolved[1].0["url"],
            "https://repo.example/maven/org/example/binding/1.2/binding-1.2-natives-windows-64.jar"
        );
        assert!(resolved[1].0.get("sha1").is_none());
        assert!(resolved[1].0.get("size").is_none());
    }
    #[test]
    fn modern_paths_can_be_derived_while_native_integrity_is_retained() {
        let library = json!({"name":"a.b:c:1", "downloads":{
            "artifact":{"url":"https://example/main.jar"},
            "classifiers":{"natives-linux":{"url":"https://example/native.jar", "sha1":"native-hash", "size":7}}
        }, "natives":{"linux":"natives-linux"}});
        let resolved = LibraryResolver::new("linux", "64")
            .resolve(&library)
            .unwrap();
        assert_eq!(resolved[0].0["path"], "a/b/c/1/c-1.jar");
        assert_eq!(resolved[1].0["path"], "a/b/c/1/c-1-natives-linux.jar");
        assert_eq!(resolved[1].0["sha1"], "native-hash");
        assert_eq!(resolved[1].0["size"], 7);
    }
    #[test]
    fn rejects_missing_modern_classifier_and_unsafe_paths_before_download() {
        let resolver = LibraryResolver::new("osx", "64");
        assert!(resolver
            .resolve(&json!({"name":"a:b:1", "downloads":{}, "natives":{"osx":"natives-osx"}}))
            .is_err());
        assert!(resolver.resolve(&json!({"downloads":{"artifact":{"path":"../escape.jar", "url":"https://example/a"}}})).is_err());
        assert!(resolver
            .resolve(&json!({"name":"a:b:1", "natives":{"osx":"../../bad"}}))
            .is_err());
    }
    #[test]
    fn upgrades_legacy_official_repository_and_skips_other_platform_natives() {
        let entries = LibraryResolver::new("linux", "64").resolve(&json!({
            "name":"a:b:1", "url":"http://libraries.minecraft.net/", "natives":{"windows":"natives-windows"}
        })).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].0["url"],
            "https://libraries.minecraft.net/a/b/1/b-1.jar"
        );
    }
}
