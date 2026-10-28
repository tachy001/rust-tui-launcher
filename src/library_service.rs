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
        for (path, label) in [
            ("/downloads", "downloads"),
            ("/downloads/classifiers", "downloads.classifiers"),
            ("/natives", "natives"),
        ] {
            if library
                .pointer(path)
                .is_some_and(|value| !value.is_null() && !value.is_object())
            {
                bail!("依赖库 {label} 必须为 JSON 对象");
            }
        }
        if let Some(classifier) = library["natives"].get(self.os) {
            if !classifier.is_null() && classifier.as_str().is_none_or(str::is_empty) {
                bail!("当前平台 native 分类必须为非空字符串：{}", self.os);
            }
        }
        let mut entries = vec![];
        if let Some(artifact) = library
            .pointer("/downloads/artifact")
            .filter(|value| !value.is_null())
        {
            entries.push((self.complete(artifact, library, None)?, false));
        } else if library["downloads"].is_null() && library["natives"].is_null() {
            entries.push((self.legacy(library, None)?, false));
        }
        if let Some(classifier) = library["natives"][self.os].as_str() {
            let classifier = classifier.replace("${arch}", self.arch);
            let entry = match library
                .pointer("/downloads/classifiers")
                .and_then(|items| items.get(&classifier))
            {
                Some(entry) => self.complete(entry, library, Some(&classifier))?,
                None if library["downloads"].is_null() => {
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
        let url = self.repository_url(library, &path)?;
        let mut entry = json!({"path":path, "url":url});
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
    fn repository_url(&self, library: &Value, path: &str) -> Result<String> {
        let base = match library.get("url") {
            None | Some(Value::Null) => "https://libraries.minecraft.net/",
            Some(Value::String(url)) => url.as_str(),
            _ => bail!("Maven 仓库地址必须为字符串"),
        };
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
        // Maven coordinates describe literal path segments. Concatenating a
        // URL string would interpret '#' as a fragment and '%' as an escape.
        repository
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Maven 仓库地址不支持路径"))?
            .pop_if_empty()
            .extend(path.split('/'));
        Ok(repository.into())
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
            Some(Value::String(path)) => path.replace('\\', "/"),
            None => self.path(library, classifier)?,
            _ => bail!("依赖路径必须为字符串"),
        };
        safe_path(Path::new("libraries"), &path)?;
        match entry.get("url") {
            None | Some(Value::Null) => entry["url"] = json!(self.repository_url(library, &path)?),
            // Forge processors publish local-only artifacts with an explicit
            // empty URL. Preserve it: missing URLs alone use repository fallback.
            Some(Value::String(_)) => {}
            _ => bail!("资源下载地址必须为字符串"),
        }
        entry["path"] = json!(path);
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installer_generated_artifact_keeps_explicit_empty_url() {
        let entries = LibraryResolver::new("windows", "64")
            .resolve(&json!({"name":"net.minecraftforge:forge:1.21.1-52.1.20:client",
                "downloads":{"artifact":{"url":"","sha1":"a9993e364706816aba3e25717850c26c9cd0d89d","size":3}}}))
            .unwrap();
        assert_eq!(entries[0].0["url"], "");
        assert_eq!(entries[0].0["size"], 3);
        assert!(entries[0].0["path"]
            .as_str()
            .unwrap()
            .ends_with("-client.jar"));
    }
    #[test]
    fn windows_metadata_paths_share_one_cache_and_download_layout() {
        let resolver = LibraryResolver::new("linux", "64");
        let entries = resolver
            .resolve(&json!({"downloads":{"artifact":{
            "path":"org\\example\\binding\\1\\binding-1.jar"}}}))
            .unwrap();
        let artifact = &entries[0].0;
        assert_eq!(artifact["path"], "org/example/binding/1/binding-1.jar");
        assert_eq!(
            artifact["url"],
            "https://libraries.minecraft.net/org/example/binding/1/binding-1.jar"
        );
        assert_eq!(
            safe_path(Path::new("libraries"), artifact["path"].as_str().unwrap()).unwrap(),
            Path::new("libraries").join("org/example/binding/1/binding-1.jar")
        );
        assert!(resolver
            .resolve(&json!({"downloads":{"artifact":{"path":"org\\..\\escape.jar"}}}))
            .is_err());
    }
    #[test]
    fn modern_downloads_fall_back_to_repository_and_keep_integrity() {
        let resolver = LibraryResolver::new("linux", "64");
        let library = json!({"name":"a.b:binding:1", "url":"https://repo.example/maven",
            "downloads":{"artifact":{"path":"custom/main.jar","sha1":"main-hash","size":42},
                "classifiers":{"natives-linux":{"url":null,"sha1":"native-hash","size":7}}},
            "natives":{"linux":"natives-linux"}});
        let entries = resolver.resolve(&library).unwrap();
        assert_eq!(
            entries[0].0["url"],
            "https://repo.example/maven/custom/main.jar"
        );
        assert_eq!(entries[0].0["sha1"], "main-hash");
        assert_eq!(entries[0].0["size"], 42);
        assert_eq!(
            entries[1].0["url"],
            "https://repo.example/maven/a/b/binding/1/binding-1-natives-linux.jar"
        );
        assert_eq!(entries[1].0["sha1"], "native-hash");
        assert_eq!(entries[1].0["size"], 7);
        assert!(entries[1].1);
        let default = resolver
            .resolve(&json!({"downloads":{"artifact":{"path":"custom/main.jar"}}}))
            .unwrap();
        assert_eq!(
            default[0].0["url"],
            "https://libraries.minecraft.net/custom/main.jar"
        );
        let explicit = resolver.resolve(&json!({"url":42,"downloads":{"artifact":{"path":"main.jar","url":"https://cdn.example/file"}}})).unwrap();
        assert_eq!(explicit[0].0["url"], "https://cdn.example/file");
        for url in [json!(42), json!(""), json!("http://insecure.example/")] {
            assert!(resolver
                .resolve(&json!({"url":url,"downloads":{"artifact":{"path":"main.jar"}}}))
                .is_err());
        }
    }
    #[test]
    fn legacy_repository_encodes_literal_coordinates_without_changing_cache_path() {
        for base in ["https://repo.example/maven", "https://repo.example/maven/"] {
            let entries = LibraryResolver::new("linux", "64")
                .resolve(&json!({"name":"org.example:lib#part:1%20two", "url":base}))
                .unwrap();
            assert_eq!(
                entries[0].0["path"],
                "org/example/lib#part/1%20two/lib#part-1%20two.jar"
            );
            let url = reqwest::Url::parse(entries[0].0["url"].as_str().unwrap()).unwrap();
            assert!(url.fragment().is_none());
            assert!(url.query().is_none());
            assert_eq!(
                url.path(),
                "/maven/org/example/lib%23part/1%2520two/lib%23part-1%2520two.jar"
            );
        }
    }
    #[test]
    fn malformed_library_maps_cannot_silently_remove_dependencies() {
        let resolver = LibraryResolver::new("linux", "64");
        for fields in [
            json!({"downloads":[]}),
            json!({"downloads":"broken"}),
            json!({"downloads":{"classifiers":[]}}),
            json!({"natives":false}),
            json!({"natives":{"linux":42}}),
            json!({"natives":{"linux":""}}),
        ] {
            let mut library = json!({"name":"test:required:1"});
            library
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert!(resolver.resolve(&library).is_err(), "{library}");
        }
        assert!(resolver.resolve(&json!({"name":"test:other:1", "natives":{"windows":"natives-windows","linux":null}})).unwrap().is_empty());
    }
    #[test]
    fn null_legacy_optional_fields_are_absent_and_native_only_libraries_need_no_main_jar() {
        let resolver = LibraryResolver::new("windows", "64");
        assert_eq!(
            resolver
                .resolve(&json!({"name":"test:api:1","downloads":null,"natives":null}))
                .unwrap()
                .len(),
            1
        );
        let native = resolver
            .resolve(&json!({"name":"test:platform:1","downloads":null,
            "natives":{"windows":"natives-windows-${arch}"}}))
            .unwrap();
        assert_eq!(native.len(), 1);
        assert!(native[0].1);
        assert_eq!(
            native[0].0["path"],
            "test/platform/1/platform-1-natives-windows-64.jar"
        );
    }
    #[test]
    fn legacy_natives_use_the_classifier_and_do_not_reuse_main_jar_checksums() {
        let library = json!({"name":"org.example:binding:1.2", "url":"https://repo.example/maven/",
            "sha1":"main-hash", "size":42, "natives":{"windows":"natives-windows-${arch}"}});
        let resolved = LibraryResolver::new("windows", "64")
            .resolve(&library)
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].1);
        assert_eq!(
            resolved[0].0["path"],
            "org/example/binding/1.2/binding-1.2-natives-windows-64.jar"
        );
        assert_eq!(
            resolved[0].0["url"],
            "https://repo.example/maven/org/example/binding/1.2/binding-1.2-natives-windows-64.jar"
        );
        assert!(resolved[0].0.get("sha1").is_none());
        assert!(resolved[0].0.get("size").is_none());
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
        assert!(entries.is_empty());
        let entries = LibraryResolver::new("linux", "64")
            .resolve(&json!({
                "name":"a:b:1", "url":"http://libraries.minecraft.net/"
            }))
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].0["url"],
            "https://libraries.minecraft.net/a/b/1/b-1.jar"
        );
    }
}
