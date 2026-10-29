use crate::{
    backend::Backend,
    config::Config,
    metadata::{valid_id, Manifest, RemoteVersion, MANIFEST},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use std::fs;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Loader {
    Fabric,
    Quilt,
}
impl Loader {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
        }
    }
    fn base(self) -> &'static str {
        match self {
            Self::Fabric => "https://meta.fabricmc.net/v2",
            Self::Quilt => "https://meta.quiltmc.org/v3",
        }
    }
}
impl Backend {
    pub fn manifest(&self) -> Result<Manifest> {
        self.cached_metadata(MANIFEST, parse_manifest)
    }
    pub fn remote_version(&self, id: &str) -> Result<RemoteVersion> {
        valid_id(id)?;
        self.cached_metadata(MANIFEST, |bytes| parse_remote_version(bytes, id))
    }
    pub fn loader_profile(
        &self,
        loader: Loader,
        game: &str,
        requested: Option<&str>,
    ) -> Result<Value> {
        valid_id(game)?;
        if let Some(version) = requested {
            valid_id(version).context("加载器版本号无效")?;
        }
        let client = reqwest::Url::parse(loader.base())?;
        let mut list_url = client.clone();
        list_url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("无效 API 地址"))?
            .extend(["versions", "loader", game]);
        let list = self.cached_metadata(list_url.as_str(), |bytes| {
            crate::loader_catalog::json_versions(serde_json::from_slice(bytes)?)
        })?;
        let chosen = crate::loader_catalog::LoaderVersion::select(&list, requested)?;
        let version = chosen.version.as_str();
        self.log(format!("{} {version} / Minecraft {game}", loader.name()));
        let mut profile_url = client;
        profile_url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("无效 API 地址"))?
            .extend(["versions", "loader", game, version, "profile", "json"]);
        self.cached_metadata(profile_url.as_str(), |bytes| parse_profile(bytes, game))
    }
    pub fn install_loader(
        &self,
        config: &Config,
        loader: Loader,
        game: &str,
        version: Option<&str>,
    ) -> Result<String> {
        let profile = self.loader_profile(loader, game, version)?;
        let remote = self.remote_version(game)?;
        self.install(config, &remote)?;
        self.control.check()?;
        let id = profile["id"].as_str().context("加载器配置缺少 ID")?;
        let mut mutation = crate::instance_service::InstanceMutation::begin(&config.root()?, id)?;
        self.install_profile_with_session(config, id, profile.clone(), &mut mutation)?;
        self.log(format!("{} 实例已就绪：{id}", loader.name()));
        Ok(id.to_owned())
    }
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    if manifest.versions.is_empty() {
        bail!("官方版本目录为空");
    }
    let mut seen = std::collections::HashSet::new();
    for version in &manifest.versions {
        valid_id(&version.id)?;
        if !seen.insert(&version.id) {
            bail!("官方目录包含重复版本 ID");
        }
        crate::metadata_cache::validate_url(&version.url).context("版本下载地址无效")?;
        if version.kind.is_empty()
            || version.sha1.len() != 40
            || !version.sha1.bytes().all(|c| c.is_ascii_hexdigit())
        {
            bail!("版本目录缺少有效类型或 SHA-1");
        }
    }
    Ok(manifest)
}
fn parse_remote_version(bytes: &[u8], id: &str) -> Result<RemoteVersion> {
    parse_manifest(bytes)?
        .versions
        .into_iter()
        .find(|version| version.id == id)
        .with_context(|| format!("版本目录中不存在请求的版本 {id}"))
}
fn parse_profile(bytes: &[u8], game: &str) -> Result<Value> {
    let profile: Value = serde_json::from_slice(bytes)?;
    if profile["inheritsFrom"] != game
        || !profile["mainClass"].as_str().is_some_and(|value| {
            !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        || !profile["libraries"].is_array()
    {
        bail!("加载器 API 返回了不兼容的启动配置");
    }
    let id = profile["id"].as_str().context("加载器配置缺少 ID")?;
    valid_id(id)?;
    if id == game {
        bail!("加载器实例 ID 不能覆盖原版父版本");
    }
    for library in profile["libraries"].as_array().unwrap() {
        crate::version::maven_path(
            library["name"]
                .as_str()
                .context("加载器依赖缺少 Maven 坐标")?,
        )?;
    }
    crate::version_profile_service::VersionProfileDecoder
        .decode(profile.clone(), id)
        .context("加载器 API 返回了无效的版本配置")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_manifest_validation_rejects_duplicates_and_unusable_entries() {
        let entry = serde_json::json!({"id":"1.21.1","type":"release","url":"https://example.invalid/version.json","sha1":"a".repeat(40)});
        let bytes = serde_json::to_vec(&serde_json::json!({"versions":[entry.clone()]})).unwrap();
        assert!(parse_remote_version(&bytes, "1.21.1").is_ok());
        assert!(parse_remote_version(&bytes, "1.20.1").is_err());
        assert_eq!(
            parse_manifest(
                &serde_json::to_vec(&serde_json::json!({"versions":[entry.clone()]})).unwrap()
            )
            .unwrap()
            .versions
            .len(),
            1
        );
        assert!(parse_manifest(
            &serde_json::to_vec(&serde_json::json!({"versions":[entry.clone(),entry.clone()]}))
                .unwrap()
        )
        .is_err());
        for (key, value) in [
            ("id", "../bad"),
            ("sha1", "bad"),
            ("url", "http://example.invalid/version.json"),
        ] {
            let mut broken = entry.clone();
            broken[key] = value.into();
            assert!(parse_manifest(
                &serde_json::to_vec(&serde_json::json!({"versions":[broken]})).unwrap()
            )
            .is_err());
        }
    }
    #[test]
    fn cached_profile_is_validated_for_the_requested_parent_before_persisting() {
        let mut profile = serde_json::json!({"id":"fabric-test","inheritsFrom":"1.21.1","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.17.0"}]});
        let bytes = serde_json::to_vec(&profile).unwrap();
        assert!(parse_profile(&bytes, "1.21.1").is_ok());
        assert!(parse_profile(&bytes, "1.20.1").is_err());
        for (field, value) in [
            ("id", serde_json::json!("1.21.1")),
            ("arguments", serde_json::json!({"jvm": "-Xmx2G"})),
            ("javaVersion", serde_json::json!({"majorVersion": 0})),
            (
                "libraries",
                serde_json::json!([{"name":"net.fabricmc:fabric-loader:0.17.0","rules":"allow"}]),
            ),
        ] {
            let mut invalid = profile.clone();
            invalid[field] = value;
            assert!(
                parse_profile(&serde_json::to_vec(&invalid).unwrap(), "1.21.1").is_err(),
                "{field}"
            );
        }
        for main in ["", "main class", "main\0class"] {
            let mut invalid = profile.clone();
            invalid["mainClass"] = main.into();
            assert!(parse_profile(&serde_json::to_vec(&invalid).unwrap(), "1.21.1").is_err());
        }
        profile["libraries"][0]["name"] = "../bad".into();
        assert!(parse_profile(&serde_json::to_vec(&profile).unwrap(), "1.21.1").is_err());
    }
    #[test]
    fn old_manifest_cache_remains_usable_offline_with_current_validation() {
        struct Offline;
        impl crate::metadata_cache::MetadataSource for Offline {
            fn fetch(&self, _: &str) -> Result<Vec<u8>> {
                bail!("offline")
            }
        }
        let workspace = crate::test_support::Workspace::new();
        let body=serde_json::to_vec(&serde_json::json!({"versions":[{"id":"1.21.1","type":"release","url":"https://example.invalid/version.json","sha1":"a".repeat(40)}]})).unwrap();
        fs::write(workspace.path.join("versions.json"), body).unwrap();
        let store = crate::metadata_cache::FileMetadataStore::new(&workspace.path);
        let control = crate::task::TaskControl::default();
        let cache = crate::metadata_cache::MetadataCache::new(&Offline, &store, &control);
        let result = cache
            .load(MANIFEST, |bytes| parse_remote_version(bytes, "1.21.1"))
            .unwrap();
        assert_eq!(result.value.id, "1.21.1");
        assert!(result.warning.is_some());
        assert!(cache
            .load(MANIFEST, |bytes| parse_remote_version(bytes, "missing"))
            .is_err());
    }
    #[test]
    fn malformed_online_loader_profile_preserves_valid_cached_profile() {
        use crate::metadata_cache::{MetadataCache, MetadataSource, MetadataStore};
        struct Source(Vec<u8>);
        impl MetadataSource for Source {
            fn fetch(&self, _: &str) -> Result<Vec<u8>> {
                Ok(self.0.clone())
            }
        }
        struct Store(Vec<u8>);
        impl MetadataStore for Store {
            fn read(&self, _: &str) -> Result<Option<Vec<u8>>> {
                Ok(Some(self.0.clone()))
            }
            fn write(&self, _: &str, _: &[u8]) -> Result<()> {
                panic!("invalid online profile must not overwrite cached data")
            }
        }
        let valid = serde_json::json!({"id":"fabric-test","inheritsFrom":"1.21.1","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.17.0"}]});
        let mut invalid = valid.clone();
        invalid["arguments"] = serde_json::json!({"game": false});
        let source = Source(serde_json::to_vec(&invalid).unwrap());
        let store = Store(serde_json::to_vec(&valid).unwrap());
        let control = crate::task::TaskControl::default();
        let cache = MetadataCache::new(&source, &store, &control);
        let url = "https://meta.fabricmc.net/v2/versions/loader/1.21.1/0.17.0/profile/json";
        let result = cache
            .load(url, |bytes| parse_profile(bytes, "1.21.1"))
            .unwrap();
        assert_eq!(result.value, valid);
        assert!(result.warning.is_some());
        assert!(cache
            .load(url, |bytes| parse_profile(bytes, "1.20.1"))
            .is_err());
        control.cancel();
        assert!(cache
            .load(url, |bytes| parse_profile(bytes, "1.21.1"))
            .is_err());
    }
    #[test]
    #[ignore = "requires official Fabric and Quilt services"]
    fn official_loader_profiles() {
        let (tx, rx) = std::sync::mpsc::channel();
        let backend = Backend::new(tx).unwrap();
        for loader in [Loader::Fabric, Loader::Quilt] {
            let profile = backend.loader_profile(loader, "1.21.1", None).unwrap();
            assert_eq!(profile["inheritsFrom"], "1.21.1");
            for library in profile["libraries"].as_array().unwrap() {
                crate::version::maven_path(library["name"].as_str().unwrap()).unwrap();
                for os in ["windows", "linux", "osx"] {
                    let resolver = crate::library_service::LibraryResolver::new(os, "64");
                    for (entry, _) in resolver.resolve(library).unwrap() {
                        crate::metadata::safe_path(
                            std::path::Path::new("libraries"),
                            entry["path"].as_str().unwrap(),
                        )
                        .unwrap();
                        let url = reqwest::Url::parse(entry["url"].as_str().unwrap()).unwrap();
                        assert_eq!(url.scheme(), "https");
                        assert!(url.host_str().is_some());
                    }
                }
            }
        }
        for message in rx.try_iter() {
            if let crate::backend::Message::Log(line) = message {
                assert!(
                    !line.starts_with("无法获取有效实时元数据，使用已校验缓存"),
                    "在线加载器验收发生缓存回退：{line}"
                );
            }
        }
    }
}
