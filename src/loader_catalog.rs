use crate::{backend::Backend, forge::ForgeKind, loader::Loader, metadata::valid_id};
use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub enum InstallerKind {
    Fabric,
    Quilt,
    Forge,
    NeoForge,
    OptiFine,
}
impl InstallerKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fabric => "Fabric",
            Self::Quilt => "Quilt",
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
            Self::OptiFine => "OptiFine",
        }
    }
    pub fn parse(name: &str) -> Result<Self> {
        match name.to_lowercase().as_str() {
            "fabric" => Ok(Self::Fabric),
            "quilt" => Ok(Self::Quilt),
            "forge" => Ok(Self::Forge),
            "neoforge" => Ok(Self::NeoForge),
            "optifine" => Ok(Self::OptiFine),
            _ => bail!("组件应为 fabric / quilt / forge / neoforge / optifine"),
        }
    }
}
#[derive(Debug)]
pub struct LoaderVersion {
    pub version: String,
    pub stable: bool,
}

pub(crate) fn json_versions(value: Value) -> Result<Vec<LoaderVersion>> {
    let mut result = vec![];
    for entry in value.as_array().context("加载器版本列表格式错误")? {
        let version = entry["loader"]["version"]
            .as_str()
            .context("加载器缺少版本号")?;
        valid_id(version)?;
        if result
            .iter()
            .any(|entry: &LoaderVersion| entry.version == version)
        {
            continue;
        }
        result.push(LoaderVersion {
            version: version.into(),
            stable: entry["loader"]["stable"]
                .as_bool()
                .unwrap_or(!version.contains("beta") && !version.contains("alpha")),
        });
    }
    Ok(result)
}

fn maven_versions(xml: &str, prefix: &str) -> Result<Vec<LoaderVersion>> {
    let re = regex::Regex::new(r"<version>\s*([^<\s]+)\s*</version>")?;
    let mut versions: Vec<_> = re
        .captures_iter(xml)
        .map(|capture| capture[1].to_owned())
        .filter(|version| version.starts_with(prefix))
        .collect();
    versions.sort_by_key(|version| crate::forge::natural_key(version));
    versions.dedup();
    versions
        .into_iter()
        .rev()
        .map(|version| {
            valid_id(&version)?;
            let stable = !version.contains("beta")
                && !version.contains("alpha")
                && !version.contains("snapshot");
            Ok(LoaderVersion { version, stable })
        })
        .collect()
}

impl Backend {
    pub fn loader_versions(&self, kind: InstallerKind, game: &str) -> Result<Vec<LoaderVersion>> {
        valid_id(game)?;
        let versions = match kind {
            InstallerKind::OptiFine => self
                .optifine_versions(game)?
                .into_iter()
                .map(|version| LoaderVersion {
                    version: version.identity.version(),
                    stable: version.stable,
                })
                .collect(),
            InstallerKind::Fabric | InstallerKind::Quilt => {
                let base = if matches!(kind, InstallerKind::Fabric) {
                    "https://meta.fabricmc.net/v2"
                } else {
                    "https://meta.quiltmc.org/v3"
                };
                let mut url = reqwest::Url::parse(base)?;
                url.path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("无效 API 地址"))?
                    .extend(["versions", "loader", game]);
                self.cached_metadata(url.as_str(), |bytes| {
                    json_versions(serde_json::from_slice(bytes)?)
                })?
            }
            InstallerKind::Forge => maven_versions(
                &self.maven_metadata(
                    "https://maven.minecraftforge.net/net/minecraftforge/forge/maven-metadata.xml",
                )?,
                &format!("{game}-"),
            )?,
            InstallerKind::NeoForge => {
                let artifact = if game == "1.20.1" {
                    "forge"
                } else {
                    "neoforge"
                };
                maven_versions(&self.maven_metadata(&format!("https://maven.neoforged.net/releases/net/neoforged/{artifact}/maven-metadata.xml"))?,
                    &crate::forge::neoforge_prefix(game)?)?
            }
        };
        if versions.is_empty() {
            bail!("{} 没有适用于 Minecraft {game} 的版本", kind.name());
        }
        Ok(versions)
    }
    pub fn install_selected_loader(
        &self,
        config: &crate::config::Config,
        kind: InstallerKind,
        game: &str,
        version: Option<&str>,
    ) -> Result<String> {
        match kind {
            InstallerKind::OptiFine => self.install_optifine(config, game, version),
            InstallerKind::Fabric => self.install_loader(config, Loader::Fabric, game, version),
            InstallerKind::Quilt => self.install_loader(config, Loader::Quilt, game, version),
            InstallerKind::Forge => self.install_forge(config, ForgeKind::Forge, game, version),
            InstallerKind::NeoForge => {
                self.install_forge(config, ForgeKind::NeoForge, game, version)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_minecraft_versions_and_sorts_build_numbers() {
        let versions = maven_versions("<versions><version>1.21.1-52.0.9</version><version>1.21-51.0.1</version><version>1.21.1-52.0.100</version><version>1.21.1-52.0.9</version></versions>", "1.21.1-").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, "1.21.1-52.0.100");
        let versions = maven_versions(
            "<version>21.1.3-beta</version><version>21.10.1</version>",
            "21.1.",
        )
        .unwrap();
        assert_eq!(versions.len(), 1);
        assert!(!versions[0].stable);
    }
    #[test]
    fn preserves_api_order_and_explicit_stability() {
        let versions = json_versions(serde_json::json!([
            {"loader":{"version":"0.17.0","stable":false}},
            {"loader":{"version":"0.16.0","stable":true}},
            {"loader":{"version":"0.16.0","stable":true}}
        ]))
        .unwrap();
        assert_eq!(versions.len(), 2);
        assert!(!versions[0].stable);
        assert!(versions[1].stable);
        assert!(json_versions(serde_json::json!([{"loader":{"version":"../bad"}}])).is_err());
    }
}
