use crate::{
    backend::Backend,
    config::Config,
    metadata::{valid_id, Manifest, RemoteVersion, MANIFEST},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
        self.control.check()?;
        let cache = std::path::Path::new(".tui-mc/cache/versions.json");
        let online = self
            .get_json(MANIFEST)
            .and_then(|value| Ok(serde_json::from_value::<Manifest>(value)?));
        match online {
            Ok(manifest) => {
                fs::create_dir_all(cache.parent().unwrap())?;
                crate::storage::atomic_write(cache, &serde_json::to_vec(&manifest)?)?;
                Ok(manifest)
            }
            Err(error) => {
                self.control.check()?;
                if let Ok(bytes) = fs::read(cache) {
                    if let Ok(manifest) = serde_json::from_slice(&bytes) {
                        self.log(format!("网络不可用，使用缓存版本目录：{error}"));
                        return Ok(manifest);
                    }
                }
                Err(error)
            }
        }
    }
    pub fn remote_version(&self, id: &str) -> Result<RemoteVersion> {
        self.manifest()?
            .versions
            .into_iter()
            .find(|v| v.id == id)
            .with_context(|| format!("官方目录中不存在版本 {id}"))
    }
    pub fn loader_profile(
        &self,
        loader: Loader,
        game: &str,
        requested: Option<&str>,
    ) -> Result<Value> {
        valid_id(game)?;
        let client = reqwest::Url::parse(loader.base())?;
        let mut list_url = client.clone();
        list_url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("无效 API 地址"))?
            .extend(["versions", "loader", game]);
        let list = self.get_json(list_url.as_str())?;
        let entries = list.as_array().context("加载器列表格式错误")?;
        let chosen = if let Some(requested) = requested {
            entries
                .iter()
                .find(|entry| entry["loader"]["version"] == requested)
        } else {
            entries
                .iter()
                .find(|entry| {
                    let version = entry["loader"]["version"].as_str().unwrap_or("");
                    entry["loader"]["stable"]
                        .as_bool()
                        .unwrap_or(!version.contains("beta") && !version.contains("alpha"))
                })
                .or_else(|| entries.first())
        }
        .context("该游戏版本没有可用的加载器版本")?;
        let version = chosen["loader"]["version"]
            .as_str()
            .context("加载器缺少版本号")?;
        self.log(format!("{} {version} / Minecraft {game}", loader.name()));
        let mut profile_url = client;
        profile_url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("无效 API 地址"))?
            .extend(["versions", "loader", game, version, "profile", "json"]);
        let profile = self.get_json(profile_url.as_str())?;
        if profile["inheritsFrom"] != game
            || !profile["mainClass"].is_string()
            || !profile["libraries"].is_array()
        {
            bail!("加载器 API 返回了不兼容的启动配置");
        }
        valid_id(profile["id"].as_str().context("加载器配置缺少 ID")?)?;
        Ok(profile)
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
        let dir = config.root()?.join("versions").join(id);
        fs::create_dir_all(&dir)?;
        crate::storage::atomic_write(
            &dir.join(format!("{id}.json")),
            &serde_json::to_vec_pretty(&profile)?,
        )?;
        self.repair(config, id)?;
        self.log(format!("{} 实例已就绪：{id}", loader.name()));
        Ok(id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires official Fabric and Quilt services"]
    fn official_loader_profiles() {
        let (tx, _) = std::sync::mpsc::channel();
        let backend = Backend::new(tx).unwrap();
        for loader in [Loader::Fabric, Loader::Quilt] {
            let profile = backend.loader_profile(loader, "1.21.1", None).unwrap();
            assert_eq!(profile["inheritsFrom"], "1.21.1");
            for library in profile["libraries"].as_array().unwrap() {
                crate::version::maven_path(library["name"].as_str().unwrap()).unwrap();
            }
        }
    }
}
