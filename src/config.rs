use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceSettings {
    pub java: Option<String>,
    pub memory_mb: Option<u32>,
    pub isolated: Option<bool>,
    pub jvm_args: Option<String>,
    pub game_args: Option<String>,
    pub fullscreen: Option<bool>,
    pub server: Option<String>,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_config_gets_new_defaults() {
        let config: Config = serde_json::from_str(
            r#"{"game_dir":".minecraft","java":"java","username":"Player","memory_mb":2048}"#,
        )
        .unwrap();
        assert_eq!(config.download_threads, 6);
        assert!(config.isolated);
        assert!(config.microsoft_client_id.is_empty());
        assert!(!config.fullscreen);
        assert!(config.server.is_empty());
    }
    #[test]
    fn instance_overrides_do_not_modify_global_settings() {
        let mut config = Config::default();
        config.instances.insert(
            "test-instance".into(),
            InstanceSettings {
                memory_mb: Some(4096),
                isolated: Some(false),
                jvm_args: Some("-Dtest=true".into()),
                ..InstanceSettings::default()
            },
        );
        let instance = config.effective("test-instance");
        assert_eq!(instance.memory_mb, 4096);
        assert!(!instance.isolated);
        assert_eq!(instance.jvm_args, "-Dtest=true");
        assert_eq!(config.memory_mb, 2048);
        assert!(config.isolated);
    }
    #[test]
    fn launch_options_can_disable_global_defaults_for_one_instance() {
        let mut config = Config {
            fullscreen: true,
            server: "example.org".into(),
            ..Config::default()
        };
        config.instances.insert(
            "test".into(),
            InstanceSettings {
                fullscreen: Some(false),
                server: Some(String::new()),
                ..InstanceSettings::default()
            },
        );
        let instance = config.effective("test");
        assert!(!instance.fullscreen);
        assert!(instance.server.is_empty());
        assert!(config.fullscreen);
        assert_eq!(config.server, "example.org");
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub game_dir: PathBuf,
    pub java: String,
    pub username: String,
    pub memory_mb: u32,
    pub download_threads: usize,
    pub isolated: bool,
    pub jvm_args: String,
    pub game_args: String,
    pub width: u32,
    pub height: u32,
    pub last_version: Option<String>,
    pub instances: HashMap<String, InstanceSettings>,
    pub microsoft_client_id: String,
    pub download_source: String,
    pub optional_pack_files: bool,
    pub fullscreen: bool,
    pub server: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            game_dir: PathBuf::from(".minecraft"),
            java: "auto".into(),
            username: "Player".into(),
            memory_mb: 2048,
            download_threads: 6,
            isolated: true,
            jvm_args: String::new(),
            game_args: String::new(),
            width: 960,
            height: 540,
            last_version: None,
            instances: HashMap::new(),
            microsoft_client_id: String::new(),
            download_source: "official".into(),
            optional_pack_files: true,
            fullscreen: false,
            server: String::new(),
        }
    }
}
impl Config {
    pub fn load() -> Result<Self> {
        if !std::path::Path::new("launcher.json").exists() {
            return Ok(Self::default());
        }
        serde_json::from_slice(&fs::read("launcher.json")?).context("launcher.json 格式错误")
    }
    pub fn save(&self) -> Result<()> {
        self.validate()?;
        crate::storage::atomic_write(
            std::path::Path::new("launcher.json"),
            &serde_json::to_vec_pretty(self)?,
        )?;
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        crate::launch_options::server_address(&self.server)?;
        crate::sources::validate(&self.download_source)?;
        if self.username.is_empty()
            || self.username.len() > 16
            || !self
                .username
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            bail!("用户名应为 1–16 位英文字母、数字或下划线");
        }
        if !(512..=65536).contains(&self.memory_mb) {
            bail!("内存应为 512–65536 MB");
        }
        if self.java.trim().is_empty() || self.game_dir.as_os_str().is_empty() {
            bail!("Java 和游戏目录不能为空");
        }
        if !(1..=16).contains(&self.download_threads) {
            bail!("并发下载线程应为 1–16");
        }
        if !(320..=16384).contains(&self.width) || !(240..=16384).contains(&self.height) {
            bail!("窗口宽高超出允许范围");
        }
        for value in [&self.jvm_args, &self.game_args] {
            if shlex::split(value).is_none() {
                bail!("自定义参数的引号不匹配");
            }
        }
        Ok(())
    }
    pub fn effective(&self, id: &str) -> Self {
        let mut result = self.clone();
        let persisted = if crate::metadata::valid_id(id).is_ok() {
            fs::read(
                self.game_dir
                    .join("versions")
                    .join(id)
                    .join("tui-mc-instance.json"),
            )
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::instances::InstanceInfo>(&bytes).ok())
            .and_then(|info| info.settings)
        } else {
            None
        };
        for instance in persisted.iter().chain(self.instances.get(id)) {
            if let Some(java) = &instance.java {
                result.java = java.clone();
            }
            if let Some(memory) = instance.memory_mb {
                result.memory_mb = memory;
            }
            if let Some(isolated) = instance.isolated {
                result.isolated = isolated;
            }
            if let Some(args) = &instance.jvm_args {
                result.jvm_args = args.clone();
            }
            if let Some(args) = &instance.game_args {
                result.game_args = args.clone();
            }
            if let Some(fullscreen) = instance.fullscreen {
                result.fullscreen = fullscreen;
            }
            if let Some(server) = &instance.server {
                result.server = server.clone();
            }
        }
        if crate::metadata::valid_id(id).is_ok() {
            let profile = self
                .game_dir
                .join("versions")
                .join(id)
                .join(format!("{id}.json"));
            if let Ok(bytes) = fs::read(profile) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if value["tuiMcIsolated"] == true {
                        result.isolated = true;
                    }
                }
            }
        }
        result
    }
    pub fn instance_dir(&self, id: &str) -> Result<PathBuf> {
        crate::metadata::valid_id(id)?;
        let root = self.root()?;
        let dir = if self.isolated {
            root.join("versions").join(id)
        } else {
            root
        };
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
    pub fn root(&self) -> Result<PathBuf> {
        fs::create_dir_all(&self.game_dir)?;
        Ok(crate::paths::for_java(&fs::canonicalize(&self.game_dir)?))
    }
}
