use crate::{
    backend::{Backend, GameProcess},
    config::Config,
    download::Download,
    logs::LogTail,
    metadata::valid_id,
    process_service::{ProcessMonitor, ProcessOutcome},
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    fs,
    process::{Command, Stdio},
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
    pub(crate) fn maven_metadata(&self, url: &str) -> Result<String> {
        self.cached_metadata(url, parse_maven_metadata)
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
                    let promos=self.cached_metadata("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json",parse_promotions)?;
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
                let metadata = self.maven_metadata(&format!("{base}/maven-metadata.xml"))?;
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
        let checksum = self.cached_metadata(&format!("{url}.sha1"), parse_installer_sha1)?;
        let root = config.root()?;
        let cache = root.join("tui-mc-installers");
        fs::create_dir_all(&cache)?;
        let source = crate::forge_installer_service::InstallerSource {
            url,
            sha1: checksum,
        };
        let installer = source.cache_path(&root)?;
        let previous = cache.join(format!("{artifact}-{full}-installer.jar"));
        if !installer.exists() && crate::download::valid_file(&previous, Some(&source.sha1), None)?
        {
            crate::storage::atomic_copy(&installer, &mut fs::File::open(previous)?)?;
        }
        self.downloader().fetch(&Download {
            url: source.url.clone(),
            dest: installer.clone(),
            sha1: Some(source.sha1.clone()),
            size: None,
            alternatives: vec![],
        })?;
        let plan = crate::forge_installer_service::ForgeInstallerPlan::inspect(
            &installer,
            game,
            (kind == ForgeKind::Forge).then_some(full.as_str()),
            &self.control,
        )?;
        let id = plan.id;
        let mut mutation = crate::instance_service::InstanceMutation::begin(&root, &id)?;
        if let Some(legacy) = plan.direct {
            self.log("检测到可直接安装的 Forge 描述，提取内置库并通过安装草稿发布");
            self.install_profile_with_session(config, &id, legacy.profile(source)?, &mut mutation)?;
            self.log(format!("{} 实例已就绪：{id}", kind.name()));
            return Ok(id);
        }
        let expected_output = plan.output.context("现代安装包缺少输出验证要求")?;
        let resolved = mutation.resolve(game)?;
        let game_plan = crate::game_java_service::GameJavaResolver::new(
            &crate::game_java_service::JarClientVersionSource,
        )
        .resolve(&root, &resolved, &self.control)?;
        let requirement = crate::installer_java_service::InstallerJavaRequirement::inspect(
            &crate::installer_java_service::JarEntryPointSource,
            &installer,
            game_plan.requirement.recommended(),
            &self.control,
        )?;
        let effective = config.effective(game)?;
        let runtime = crate::installer_java_service::InstallerJavaService::new(self)
            .resolve(&effective, &requirement)?;
        self.log(format!(
            "安装器 Java {} · {}（入口 {} 至少 Java {}，游戏至少 Java {}）",
            runtime.major(),
            runtime.path.display(),
            requirement.main_class,
            requirement.installer_minimum,
            requirement.game_minimum
        ));
        self.control.check()?;
        let shared_installation =
            std::sync::Arc::new(crate::installation_service::LoaderInstallSession::acquire(
                &root,
                &self.control,
                || {
                    self.log("等待另一个加载器安装任务释放游戏目录…");
                },
            )?);
        let mut profile_transaction =
            crate::loader_profile_service::LoaderProfileTransaction::begin(&root, &id, &mutation)?;
        let installation = (|| -> Result<()> {
            let log_path = cache.join(format!("{artifact}-{full}.log"));
            let output = fs::File::create(&log_path)?;
            let mut tail = LogTail::open(&log_path)?;
            let _ = self.tx.send(crate::backend::Message::Pauseable(false));
            let stdout = Stdio::from(output.try_clone()?);
            let stderr = Stdio::from(output);
            profile_transaction.set_installer_active(true)?;
            let mut command = Command::new(&runtime.path);
            let environment = crate::java_environment_service::JavaEnvironment;
            let removed = environment.apply(&mut command);
            if let Some(notice) = environment.notice(&removed) {
                self.log(notice);
            }
            let mut child = match GameProcess::spawn_guarded(
                command
                    .args(["-Djava.awt.headless=true", "-jar"])
                    .arg(&installer)
                    .arg("--installClient")
                    .arg(&root)
                    .current_dir(&cache)
                    .stdin(Stdio::null())
                    .stdout(stdout)
                    .stderr(stderr),
                std::sync::Arc::clone(&shared_installation),
            ) {
                Ok(child) => child,
                Err(error) => {
                    if !error.is::<crate::process_tree_service::SpawnCleanupUnconfirmed>() {
                        profile_transaction.set_installer_active(false)?;
                    }
                    return Err(error).context("无法运行官方安装器");
                }
            };
            let outcome = ProcessMonitor::new(&self.control).run(
                &mut child,
                &mut tail,
                |line| self.log(format!("安装器 · {line}")),
                |warning| self.log(warning),
            )?;
            // Monitoring returned only after containment cleanup was confirmed.
            // Release the child's copy; the outer lease still guards verification.
            drop(child);
            profile_transaction.set_installer_active(false)?;
            let status = match outcome {
                ProcessOutcome::Exited(status) => status,
                ProcessOutcome::Cancelled => bail!("安装器已由用户终止"),
            };
            if !status.success() {
                bail!(
                    "{} 安装器退出失败（{status}），日志：{}",
                    kind.name(),
                    log_path.display()
                );
            }
            expected_output
                .verify(&root, &self.control)
                .context("安装器退出成功，但加载器实例尚不完整")?;
            mutation
                .resolve(&id)
                .context("安装器没有生成完整版本信息")?;
            drop(shared_installation);
            let _ = self.tx.send(crate::backend::Message::Pauseable(true));
            self.repair_with_session(config, &id, &mut mutation)?;
            self.control.check()?;
            Ok(())
        })();
        let result = installation.and_then(|()| profile_transaction.commit());
        match result {
            Ok(warning) => {
                if let Some(warning) = warning {
                    self.log(warning);
                }
            }
            Err(error) => {
                match profile_transaction.rollback() {
                    Ok(Some(message)) => self.log(message),
                    Ok(None) => {}
                    Err(recovery) => {
                        return Err(error).context(format!(
                            "加载器安装失败，原配置恢复也失败；恢复记录已保留：{recovery:#}"
                        ))
                    }
                }
                return Err(error);
            }
        }
        self.log(format!("{} 实例已就绪：{id}", kind.name()));
        Ok(id)
    }
}
fn parse_promotions(bytes: &[u8]) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes)?;
    for version in value["promos"]
        .as_object()
        .context("Forge 推荐目录缺少 promos")?
        .values()
    {
        valid_id(version.as_str().context("Forge 推荐版本格式无效")?)?;
    }
    Ok(value)
}
fn parse_installer_sha1(bytes: &[u8]) -> Result<String> {
    let checksum = std::str::from_utf8(bytes)?
        .split_whitespace()
        .next()
        .context("安装器缺少 SHA-1")?;
    if checksum.len() != 40 || !checksum.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("官方安装器 SHA-1 响应无效");
    }
    Ok(checksum.to_ascii_lowercase())
}
fn parse_maven_metadata(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("Maven 元数据不是 UTF-8")?;
    let mut content = text.trim_start_matches('\u{feff}').trim();
    if content.starts_with("<?xml") {
        content = content
            .split_once("?>")
            .context("Maven XML 声明不完整")?
            .1
            .trim_start();
    }
    if !content
        .strip_prefix("<metadata")
        .is_some_and(|rest| rest.starts_with('>') || rest.starts_with(char::is_whitespace))
        || !content.ends_with("</metadata>")
        || !content.contains("<versioning>")
        || !content.contains("</versioning>")
        || !content.contains("<versions>")
        || !content.contains("</versions>")
    {
        bail!("Maven 版本目录格式无效");
    }
    let versions = regex::Regex::new(r"<version>\s*([^<\s]+)\s*</version>")?;
    let mut count = 0;
    for entry in versions.captures_iter(content) {
        valid_id(&entry[1])?;
        count += 1;
    }
    if count == 0 {
        bail!("Maven 版本目录为空");
    }
    Ok(text.into())
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
    fn cache_validators_reject_html_and_malformed_public_metadata() {
        let xml=br#"<?xml version="1.0"?><metadata><versioning><versions><version>21.1.264</version></versions></versioning></metadata>"#;
        assert!(parse_maven_metadata(xml).is_ok());
        for invalid in [b"<html>service unavailable</html>".as_slice(),b"<metadata><versioning><versions></versions></versioning></metadata>",b"<metadatawrong><versioning><versions><version>21.1.1</version></versions></versioning></metadata>"] {
            assert!(parse_maven_metadata(invalid).is_err());
        }
        assert!(parse_promotions(br#"{"promos":{"1.21.1-recommended":"52.1.20"}}"#).is_ok());
        assert!(parse_promotions(br#"{"error":"unavailable"}"#).is_err());
        assert!(parse_promotions(br#"{"promos":{"1.21.1-latest":"../bad"}}"#).is_err());
        let hash = "A".repeat(40);
        assert_eq!(
            parse_installer_sha1(format!("{hash}  installer.jar\n").as_bytes()).unwrap(),
            "a".repeat(40)
        );
        assert!(parse_installer_sha1(b"<html>unavailable</html>").is_err());
    }
    #[test]
    fn neoforge_version_ranges() {
        assert_eq!(neoforge_prefix("1.21.1").unwrap(), "21.1.");
        assert_eq!(neoforge_prefix("1.21").unwrap(), "21.0.");
        assert_eq!(neoforge_prefix("1.20.1").unwrap(), "1.20.1-");
        assert!(natural_key("21.1.100") > natural_key("21.1.9"));
    }
}
