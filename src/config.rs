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
    fn rejects_nul_in_both_custom_argument_fields() {
        let mut config = Config {
            jvm_args: "-Dvalue=\0".into(),
            ..Config::default()
        };
        assert!(format!("{:#}", config.validate().unwrap_err()).contains("JVM 参数"));
        config.jvm_args.clear();
        config.game_args = "--value \0".into();
        assert!(format!("{:#}", config.validate().unwrap_err()).contains("游戏参数"));
    }
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
        let instance = config.effective("test-instance").unwrap();
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
        let instance = config.effective("test").unwrap();
        assert!(!instance.fullscreen);
        assert!(instance.server.is_empty());
        assert!(config.fullscreen);
        assert_eq!(config.server, "example.org");
    }
    #[test]
    fn validates_instance_overrides_before_they_can_be_saved() {
        for settings in [
            InstanceSettings {
                memory_mb: Some(0),
                ..Default::default()
            },
            InstanceSettings {
                java: Some(String::new()),
                ..Default::default()
            },
            InstanceSettings {
                jvm_args: Some("\"unfinished".into()),
                ..Default::default()
            },
            InstanceSettings {
                game_args: Some("--value \0".into()),
                ..Default::default()
            },
            InstanceSettings {
                server: Some("host:0".into()),
                ..Default::default()
            },
        ] {
            let mut config = Config::default();
            config.instances.insert("broken".into(), settings);
            assert!(config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("实例 broken"));
        }
        let mut config = Config::default();
        config
            .instances
            .insert("../outside".into(), InstanceSettings::default());
        assert!(config.validate().is_err());
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
        let bytes = match fs::read("launcher.json") {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error).context("读取 launcher.json 失败"),
        };
        serde_json::from_slice(&bytes).context("launcher.json 格式错误")
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
        self.validate_values()?;
        let mut base = self.clone();
        base.instances.clear();
        for (id, settings) in &self.instances {
            crate::metadata::valid_id(id).with_context(|| format!("实例设置 ID 无效：{id}"))?;
            let mut effective = base.clone();
            settings.apply_to(&mut effective);
            effective
                .validate_values()
                .with_context(|| format!("实例 {id} 的设置无效"))?;
        }
        Ok(())
    }
    fn validate_values(&self) -> Result<()> {
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
        let parser = crate::argument_service::ArgumentParser::current();
        for (name, value) in [("JVM 参数", &self.jvm_args), ("游戏参数", &self.game_args)] {
            parser
                .parse(value)
                .with_context(|| format!("{name}格式错误"))?;
        }
        Ok(())
    }
    pub fn effective(&self, id: &str) -> Result<Self> {
        let source = crate::instance_config_service::FileInstanceConfigSource {
            game_dir: &self.game_dir,
        };
        crate::instance_config_service::InstanceConfigResolver::new(self, &source).resolve(id)
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
