use crate::{
    backend::{Backend, GameProcess},
    config::Config,
    download::Download,
    java,
    logs::LogTail,
    metadata::valid_id,
    storage::atomic_write,
    version,
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ForgeKind {
    Forge,
    NeoForge,
}
impl ForgeKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Forge => "Forge",
            Self::NeoForge => "NeoForge",
        }
    }
}
impl Backend {
    pub(crate) fn text(&self, url: &str) -> Result<String> {
        self.control.check()?;
        if !url.starts_with("https://") {
            bail!("安装器地址必须为 HTTPS");
        }
        Ok(self.client.get(url).send()?.error_for_status()?.text()?)
    }
    pub fn install_forge(
        &self,
        config: &Config,
        kind: ForgeKind,
        game: &str,
        requested: Option<&str>,
    ) -> Result<String> {
        valid_id(game)?;
        let (base, artifact, full) = if kind == ForgeKind::Forge {
            let chosen = match requested {
                Some(version) => version
                    .strip_prefix(&format!("{game}-"))
                    .unwrap_or(version)
                    .to_owned(),
                None => {
                    let promos=self.get_json("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json")?;
                    promos["promos"][format!("{game}-recommended")]
                        .as_str()
                        .or_else(|| promos["promos"][format!("{game}-latest")].as_str())
                        .context("该游戏版本没有推荐或最新 Forge")?
                        .into()
                }
            };
            (
                "https://maven.minecraftforge.net/net/minecraftforge/forge".to_owned(),
                "forge",
                format!("{game}-{chosen}"),
            )
        } else {
            let artifact = if game == "1.20.1" {
                "forge"
            } else {
                "neoforge"
            };
            let base = format!("https://maven.neoforged.net/releases/net/neoforged/{artifact}");
            let prefix = neoforge_prefix(game)?;
            let full = if let Some(version) = requested {
                version.to_owned()
            } else {
                let metadata = self.text(&format!("{base}/maven-metadata.xml"))?;
                let re = regex::Regex::new(r"<version>([^<]+)</version>")?;
                let mut versions = re
                    .captures_iter(&metadata)
                    .map(|c| c[1].to_owned())
                    .filter(|v| v.starts_with(&prefix))
                    .collect::<Vec<_>>();
                versions.sort_by_key(|v| natural_key(v));
                versions
                    .iter()
                    .rev()
                    .find(|v| !v.contains("beta") && !v.contains("alpha"))
                    .or_else(|| versions.last())
                    .context("该游戏版本没有适用的 NeoForge")?
                    .clone()
            };
            if !full.starts_with(&prefix) {
                bail!("NeoForge {full} 不属于 Minecraft {game}");
            }
            (base, artifact, full)
        };
        valid_id(&full)?;
        self.log(format!("安装 {} {full}", kind.name()));
        let remote = self.remote_version(game)?;
        self.install(config, &remote)?;
        let url = format!("{base}/{full}/{artifact}-{full}-installer.jar");
        let checksum = self.text(&format!("{url}.sha1"))?;
        let checksum = checksum
            .split_whitespace()
            .next()
            .context("安装器缺少 SHA-1")?;
        if checksum.len() != 40 || !checksum.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("官方安装器 SHA-1 响应无效");
        }
        let root = config.root()?;
        let cache = root.join("tui-mc-installers");
        fs::create_dir_all(&cache)?;
        let installer = cache.join(format!("{artifact}-{full}-installer.jar"));
        self.downloader().fetch(&Download {
            url,
            dest: installer.clone(),
            sha1: Some(checksum.into()),
            size: None,
            alternatives: vec![],
        })?;
        let id = installer_profile_id(&installer, game)?;
        let mut mutation = crate::instance_service::InstanceMutation::begin(&root, &id)?;
        let launcher_profiles = root.join("launcher_profiles.json");
        if !launcher_profiles.exists() {
            atomic_write(
                &launcher_profiles,
                br#"{"profiles":{},"selectedProfile":""}"#,
            )?;
        }
        let resolved = mutation.resolve(game)?;
        let required = resolved.meta["javaVersion"]["majorVersion"]
            .as_u64()
            .unwrap_or(8);
        let effective = config.effective(game);
        let runtime = match java::select(&effective, required) {
            Ok(runtime) => runtime,
            Err(error) if effective.java == "auto" => {
                self.log(format!("正在准备安装器所需 Java：{error}"));
                self.install_java_for_version(&effective, game)?
            }
            Err(error) => return Err(error),
        };
        let log_path = cache.join(format!("{artifact}-{full}.log"));
        let output = fs::File::create(&log_path)?;
        let _ = self.tx.send(crate::backend::Message::Pauseable(false));
        let child = Command::new(&runtime.path)
            .args(["-Djava.awt.headless=true", "-jar"])
            .arg(&installer)
            .arg("--installClient")
            .arg(&root)
            .current_dir(&cache)
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .spawn()
            .context("无法运行官方安装器")?;
        let mut child = GameProcess(child);
        let mut tail = LogTail::open(&log_path)?;
        let status = loop {
            for line in tail.read_lines()? {
                self.log(format!("安装器 · {line}"));
            }
            self.control.check()?;
            if let Some(status) = child.try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(100));
        };
        for line in tail.read_lines()? {
            self.log(format!("安装器 · {line}"));
        }
        if !status.success() {
            bail!(
                "{} 安装器退出失败（{status}），日志：{}",
                kind.name(),
                log_path.display()
            );
        }
        version::resolve(&root, &id).context("安装器没有生成完整版本信息")?;
        let _ = self.tx.send(crate::backend::Message::Pauseable(true));
        self.repair_with_session(config, &id, &mut mutation)?;
        self.log(format!("{} 实例已就绪：{id}", kind.name()));
        Ok(id)
    }
}
fn installer_profile_id(path: &Path, game: &str) -> Result<String> {
    let mut archive = zip::ZipArchive::new(fs::File::open(path)?)?;
    let profile = match archive.by_name("version.json") {
        Ok(mut entry) => {
            let mut bytes = vec![];
            entry.read_to_end(&mut bytes)?;
            serde_json::from_slice::<Value>(&bytes)?
        }
        Err(_) => Value::Null,
    };
    let profile = if profile.is_null() {
        let entry = archive.by_name("install_profile.json")?;
        let install: Value = serde_json::from_reader(entry)?;
        install["versionInfo"].clone()
    } else {
        profile
    };
    if profile["inheritsFrom"]
        .as_str()
        .is_some_and(|parent| parent != game)
    {
        bail!("官方安装器的父版本与选择不一致");
    }
    let id = profile["id"]
        .as_str()
        .context("无法识别安装器中的客户端版本 ID；此旧安装器可能不支持命令行安装")?;
    valid_id(id)?;
    Ok(id.into())
}
pub(crate) fn neoforge_prefix(game: &str) -> Result<String> {
    if game == "1.20.1" {
        return Ok("1.20.1-".into());
    }
    let parts = game.split('.').collect::<Vec<_>>();
    if parts.first() == Some(&"1") && parts.len() >= 2 {
        Ok(format!(
            "{}.{}.",
            parts[1],
            parts.get(2).copied().unwrap_or("0")
        ))
    } else if parts.len() >= 2 {
        Ok(format!("{}.{}.", parts[0], parts[1]))
    } else {
        bail!("无法从该游戏版本推导 NeoForge 版本范围")
    }
}
pub(crate) fn natural_key(version: &str) -> Vec<u64> {
    version
        .split(['.', '-'])
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn neoforge_version_ranges() {
        assert_eq!(neoforge_prefix("1.21.1").unwrap(), "21.1.");
        assert_eq!(neoforge_prefix("1.21").unwrap(), "21.0.");
        assert_eq!(neoforge_prefix("1.20.1").unwrap(), "1.20.1-");
        assert!(natural_key("21.1.100") > natural_key("21.1.9"));
    }
}
