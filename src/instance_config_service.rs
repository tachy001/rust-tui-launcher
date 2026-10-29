use crate::{
    config::{Config, InstanceSettings},
    instances::InstanceInfo,
};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::{fs, path::Path};

#[derive(Default)]
pub(crate) struct InstanceConfiguration {
    pub(crate) settings: Option<InstanceSettings>,
    pub(crate) force_isolated: bool,
}
pub(crate) trait InstanceConfigSource {
    fn load(&self, id: &str) -> Result<InstanceConfiguration>;
}
pub(crate) struct FileInstanceConfigSource<'a> {
    pub(crate) game_dir: &'a Path,
    pub(crate) profile: Option<&'a serde_json::Value>,
}
impl FileInstanceConfigSource<'_> {
    fn read_optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("读取实例配置失败：{}", path.display()))
            }
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("实例配置格式错误：{}", path.display()))
    }
}
impl InstanceConfigSource for FileInstanceConfigSource<'_> {
    fn load(&self, id: &str) -> Result<InstanceConfiguration> {
        crate::metadata::valid_id(id)?;
        let dir = self.game_dir.join("versions").join(id);
        let info: Option<InstanceInfo> = Self::read_optional(&dir.join("tui-mc-instance.json"))?;
        let profile: Option<serde_json::Value> = match self.profile {
            Some(value) => Some(value.clone()),
            None => Self::read_optional(&dir.join(format!("{id}.json")))?,
        };
        if profile.as_ref().is_some_and(|value| !value.is_object()) {
            anyhow::bail!(
                "实例版本配置必须为 JSON 对象：{}",
                dir.join(format!("{id}.json")).display()
            );
        }
        let force_isolated = match profile
            .as_ref()
            .and_then(|value| value.get("tuiMcIsolated"))
        {
            None | Some(serde_json::Value::Null) => false,
            Some(value) => value.as_bool().context("实例 tuiMcIsolated 必须为布尔值")?,
        };
        Ok(InstanceConfiguration {
            settings: info.and_then(|info| info.settings),
            force_isolated,
        })
    }
}

impl InstanceSettings {
    pub(crate) fn apply_to(&self, config: &mut Config) {
        if let Some(memory) = self.min_memory_mb {
            config.min_memory_mb = memory;
        }
        if let Some(auto) = self.auto_memory {
            config.auto_memory = auto;
        }
        if let Some(width) = self.width {
            config.width = width;
        }
        if let Some(height) = self.height {
            config.height = height;
        }
        if let Some(java) = &self.java {
            config.java = java.clone();
        }
        if let Some(memory) = self.memory_mb {
            config.memory_mb = memory;
        }
        if let Some(isolated) = self.isolated {
            config.isolated = isolated;
        }
        if let Some(args) = &self.jvm_args {
            config.jvm_args = args.clone();
        }
        if let Some(args) = &self.game_args {
            config.game_args = args.clone();
        }
        if let Some(fullscreen) = self.fullscreen {
            config.fullscreen = fullscreen;
        }
        if let Some(server) = &self.server {
            config.server = server.clone();
        }
        if let Some(world) = &self.quick_play_world {
            config.quick_play_world = world.clone();
        }
        if let Some(language) = &self.game_language {
            config.game_language = language.clone();
        }
        if let Some(enabled) = self.patch_natives {
            config.patch_natives = enabled;
        }
    }
}

pub(crate) struct InstanceConfigResolver<'a> {
    config: &'a Config,
    source: &'a dyn InstanceConfigSource,
}
impl<'a> InstanceConfigResolver<'a> {
    pub(crate) fn new(config: &'a Config, source: &'a dyn InstanceConfigSource) -> Self {
        Self { config, source }
    }
    pub(crate) fn resolve(&self, id: &str) -> Result<Config> {
        crate::metadata::valid_id(id)?;
        let persisted = self.source.load(id)?;
        let mut config = self.config.clone();
        for settings in persisted
            .settings
            .iter()
            .chain(self.config.instances.get(id))
        {
            settings.apply_to(&mut config);
        }
        if persisted.force_isolated {
            config.isolated = true;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_quick_play_target_preserves_inheritance_and_rejects_conflicts() {
        let mut config = Config {
            server: "global.example".into(),
            ..Config::default()
        };
        InstanceSettings {
            quick_play_world: Some("My World".into()),
            ..Default::default()
        }
        .apply_to(&mut config);
        assert_eq!(config.server, "global.example");
        assert_eq!(config.quick_play_world, "My World");
        assert!(config.validate().is_err());
        InstanceSettings {
            server: Some("instance.example".into()),
            quick_play_world: Some(String::new()),
            ..Default::default()
        }
        .apply_to(&mut config);
        assert!(config.quick_play_world.is_empty());
        config.validate().unwrap();
        InstanceSettings {
            server: Some("conflict.example".into()),
            quick_play_world: Some("My World".into()),
            ..Default::default()
        }
        .apply_to(&mut config);
        assert!(config.validate().is_err());
    }
    struct Source(InstanceConfiguration);
    impl InstanceConfigSource for Source {
        fn load(&self, _id: &str) -> Result<InstanceConfiguration> {
            Ok(InstanceConfiguration {
                settings: self.0.settings.clone(),
                force_isolated: self.0.force_isolated,
            })
        }
    }
    #[test]
    fn explicit_false_and_empty_values_override_persisted_then_global_settings() {
        let mut config = Config {
            fullscreen: true,
            server: "global.example".into(),
            ..Config::default()
        };
        let source = Source(InstanceConfiguration {
            settings: Some(InstanceSettings {
                memory_mb: Some(4096),
                server: Some("persisted.example".into()),
                isolated: Some(true),
                ..Default::default()
            }),
            force_isolated: false,
        });
        config.instances.insert(
            "test".into(),
            InstanceSettings {
                fullscreen: Some(false),
                patch_natives: Some(false),
                server: Some(String::new()),
                isolated: Some(false),
                ..Default::default()
            },
        );
        let effective = InstanceConfigResolver::new(&config, &source)
            .resolve("test")
            .unwrap();
        assert_eq!(effective.memory_mb, 4096);
        assert!(!effective.fullscreen);
        assert!(!effective.patch_natives);
        assert!(!effective.isolated);
        assert!(effective.server.is_empty());
        assert_eq!(config.memory_mb, 2048);
        assert_eq!(config.server, "global.example");
    }
    #[test]
    fn required_isolation_wins_over_all_user_overrides() {
        let mut config = Config {
            isolated: false,
            ..Config::default()
        };
        config.instances.insert(
            "test".into(),
            InstanceSettings {
                isolated: Some(false),
                ..Default::default()
            },
        );
        let source = Source(InstanceConfiguration {
            force_isolated: true,
            ..Default::default()
        });
        assert!(
            InstanceConfigResolver::new(&config, &source)
                .resolve("test")
                .unwrap()
                .isolated
        );
    }
    #[test]
    fn automatic_memory_round_trip_preserves_explicit_false_and_inheritance() {
        let mut config = Config {
            auto_memory: true,
            ..Default::default()
        };
        let source = Source(InstanceConfiguration {
            settings: Some(
                serde_json::from_str(r#"{"auto_memory":false,"memory_mb":3072}"#).unwrap(),
            ),
            force_isolated: false,
        });
        let inherited = InstanceConfigResolver::new(&config, &source)
            .resolve("test")
            .unwrap();
        assert!(!inherited.auto_memory);
        assert_eq!(inherited.memory_mb, 3072);
        config.instances.insert(
            "test".into(),
            InstanceSettings {
                auto_memory: Some(true),
                ..Default::default()
            },
        );
        let restored: Config =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        let overridden = InstanceConfigResolver::new(&restored, &source)
            .resolve("test")
            .unwrap();
        assert!(overridden.auto_memory);
        assert_eq!(overridden.memory_mb, 3072);
        config.instances.get_mut("test").unwrap().auto_memory = Some(false);
        let no_local = Source(InstanceConfiguration::default());
        assert!(
            !InstanceConfigResolver::new(&config, &no_local)
                .resolve("test")
                .unwrap()
                .auto_memory
        );
        config.instances.clear();
        assert!(
            InstanceConfigResolver::new(&config, &no_local)
                .resolve("test")
                .unwrap()
                .auto_memory
        );
        assert!(config.auto_memory);
    }
    #[test]
    fn source_failure_is_propagated_and_invalid_id_never_reaches_source() {
        struct Broken;
        impl InstanceConfigSource for Broken {
            fn load(&self, id: &str) -> Result<InstanceConfiguration> {
                assert_eq!(id, "test");
                anyhow::bail!("provider failure");
            }
        }
        let config = Config::default();
        let resolver = InstanceConfigResolver::new(&config, &Broken);
        assert!(resolver
            .resolve("test")
            .err()
            .unwrap()
            .to_string()
            .contains("provider failure"));
        assert!(resolver.resolve("../escape").is_err());
    }
    #[test]
    fn malformed_instance_settings_block_mod_operations_before_directory_creation() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            isolated: false,
            ..Default::default()
        };
        let dir = workspace.path.join("versions/test");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tui-mc-instance.json"), b"{broken").unwrap();
        assert!(crate::mods::list(&config, "test").is_err());
        assert!(!workspace.path.join("mods").exists());
        assert!(!dir.join("mods").exists());
    }
    #[test]
    fn missing_files_inherit_but_unreadable_or_bad_profile_does_not() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            ..Default::default()
        };
        assert_eq!(config.effective("test").unwrap().memory_mb, 2048);
        let dir = workspace.path.join("versions/test");
        fs::create_dir_all(dir.join("tui-mc-instance.json")).unwrap();
        assert!(config.effective("test").is_err());
        fs::remove_dir(dir.join("tui-mc-instance.json")).unwrap();
        fs::write(dir.join("test.json"), b"{broken").unwrap();
        assert!(config.effective("test").is_err());
        fs::write(dir.join("test.json"), br#"{"tuiMcIsolated":"false"}"#).unwrap();
        assert!(config.effective("test").is_err());
    }
    #[test]
    fn file_provider_reads_settings_and_required_isolation_for_the_selected_instance() {
        let workspace = crate::test_support::Workspace::new();
        let mut config = Config {
            game_dir: workspace.path.clone(),
            isolated: false,
            ..Default::default()
        };
        let dir = workspace.path.join("versions/test");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("tui-mc-instance.json"),
            br#"{"settings":{"memory_mb":4096,"isolated":false,"server":"persisted.example"}}"#,
        )
        .unwrap();
        fs::write(
            dir.join("test.json"),
            br#"{"id":"test","tuiMcIsolated":true}"#,
        )
        .unwrap();
        config.instances.insert(
            "test".into(),
            InstanceSettings {
                server: Some(String::new()),
                ..Default::default()
            },
        );
        let resolved = config.effective("test").unwrap();
        resolved.validate().unwrap();
        assert_eq!(resolved.memory_mb, 4096);
        assert!(resolved.isolated);
        assert!(resolved.server.is_empty());
        assert_eq!(config.effective("other").unwrap().memory_mb, 2048);
        assert!(!config.effective("other").unwrap().isolated);
    }
}
