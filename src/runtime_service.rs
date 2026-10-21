use crate::{backend::Backend, config::Config, java::JavaRuntime};
use anyhow::{bail, Context, Result};
use serde_json::Value;

/// A recommended major version can also have mandatory loader compatibility limits.
pub struct JavaRequirement {
    recommended: u64,
    maximum: Option<JavaCeiling>,
}
struct JavaCeiling {
    major: u64,
    component: &'static str,
}
impl JavaRequirement {
    fn library_version(library: &Value, group: &str, artifact: &str) -> Option<Vec<u64>> {
        if !crate::metadata::allowed(library) {
            return None;
        }
        let name = library["name"].as_str()?;
        let parts: Vec<_> = name.split(':').collect();
        if parts.len() != 3
            || parts[0] != group
            || parts[1] != artifact
            || !parts[2]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            return None;
        }
        parts[2]
            .split('.')
            .map(str::parse::<u64>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()
    }
    pub fn from_metadata(meta: &Value) -> Result<Self> {
        let recommended = match meta.pointer("/javaVersion/majorVersion") {
            None => 8,
            Some(value) => value
                .as_u64()
                .filter(|major| *major > 0)
                .ok_or_else(|| anyhow::anyhow!("版本元数据中的 Java majorVersion 必须为正整数"))?,
        };
        // HMCL's LAUNCH_WRAPPER constraint: old LaunchWrapper assumes URLClassLoader.
        // Inspect the launcher and library identity, not an instance's editable name.
        let legacy_wrapper = meta["mainClass"] == "net.minecraft.launchwrapper.Launch"
            && meta["libraries"].as_array().is_some_and(|libraries| {
                libraries.iter().any(|library| {
                    let Some(version) =
                        Self::library_version(library, "net.minecraft", "launchwrapper")
                    else {
                        return false;
                    };
                    version.first().is_some_and(|major| {
                        *major == 0 || (*major == 1 && version.get(1).copied().unwrap_or(0) < 13)
                    })
                })
            });
        // PCL GetJavaRequirement: Fabric Loader before 0.17.0 ships Mixin/ASM
        // that cannot handle Java 25. An inert library must not limit vanilla.
        let old_fabric = matches!(
            meta["mainClass"].as_str(),
            Some(
                "net.fabricmc.loader.impl.launch.knot.KnotClient"
                    | "net.fabricmc.loader.launch.knot.KnotClient"
            )
        ) && meta["libraries"].as_array().is_some_and(|libraries| {
            libraries.iter().any(|library| {
                Self::library_version(library, "net.fabricmc", "fabric-loader").is_some_and(
                    |version| {
                        version.first() == Some(&0) && version.get(1).copied().unwrap_or(0) < 17
                    },
                )
            })
        });
        let maximum = if legacy_wrapper {
            Some(JavaCeiling {
                major: 8,
                component: "旧 LaunchWrapper",
            })
        } else if old_fabric {
            Some(JavaCeiling {
                major: 24,
                component: "Fabric Loader 0.17.0 之前的版本",
            })
        } else {
            None
        };
        if let Some(maximum) = &maximum {
            if recommended > maximum.major {
                bail!("版本 Java 要求冲突：元数据要求 Java {recommended}，{} 最高支持 Java {}；请升级加载器或修正实例配置", maximum.component, maximum.major);
            }
        }
        Ok(Self {
            recommended,
            maximum,
        })
    }
    pub fn recommended(&self) -> u64 {
        self.recommended
    }
    fn check(&self, runtime: JavaRuntime) -> Result<JavaRuntime> {
        if runtime.major() < self.recommended {
            bail!(
                "需要 Java {}，当前 Java {}",
                self.recommended,
                runtime.major()
            );
        }
        if let Some(maximum) = &self.maximum {
            if runtime.major() > maximum.major {
                bail!(
                    "{} 不兼容 Java {}，最高支持 Java {}；建议为此实例使用 Java {}",
                    maximum.component,
                    runtime.major(),
                    maximum.major,
                    self.recommended
                );
            }
        }
        Ok(runtime)
    }
    #[cfg(test)]
    fn minimum(recommended: u64) -> Self {
        Self {
            recommended,
            maximum: None,
        }
    }
}

/// Runtime discovery and provisioning are replaceable independently of policy.
pub trait JavaRuntimeSource {
    fn check_task(&self) -> Result<()> {
        Ok(())
    }
    fn select(&self, config: &Config, required: u64) -> Result<JavaRuntime>;
    fn install(&self, config: &Config, instance: &str, required: u64) -> Result<JavaRuntime>;
}

impl JavaRuntimeSource for Backend {
    fn check_task(&self) -> Result<()> {
        self.control.check()
    }
    fn select(&self, config: &Config, required: u64) -> Result<JavaRuntime> {
        crate::java::select(config, required)
    }
    fn install(&self, config: &Config, instance: &str, _required: u64) -> Result<JavaRuntime> {
        self.install_java_for_version(config, instance)
    }
}

pub struct RuntimeResolution {
    runtime: JavaRuntime,
    warning: Option<String>,
}
impl RuntimeResolution {
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
    pub fn into_runtime(self) -> JavaRuntime {
        self.runtime
    }
}

/// Owns the explicit-path / recommended-runtime / fallback policy.
pub struct JavaRuntimeService<'a> {
    source: &'a dyn JavaRuntimeSource,
}
impl<'a> JavaRuntimeService<'a> {
    pub fn new(source: &'a dyn JavaRuntimeSource) -> Self {
        Self { source }
    }
    pub fn resolve(
        &self,
        config: &Config,
        instance: &str,
        requirement: JavaRequirement,
    ) -> Result<RuntimeResolution> {
        self.source.check_task()?;
        let required = requirement.recommended();
        let selected = self
            .source
            .select(config, required)
            .and_then(|runtime| requirement.check(runtime));
        self.source.check_task()?;
        if config.java != "auto"
            || selected
                .as_ref()
                .is_ok_and(|runtime| runtime.major() == required)
        {
            return Ok(RuntimeResolution {
                runtime: selected?,
                warning: None,
            });
        }
        let installed = self
            .source
            .install(config, instance, required)
            .and_then(|runtime| {
                if runtime.major() != required {
                    bail!(
                        "下载后的 Java 版本不符：预期 {required}，实际 {}",
                        runtime.major()
                    );
                }
                requirement.check(runtime)
            });
        self.source.check_task()?;
        match installed {
            Ok(runtime) => Ok(RuntimeResolution {
                runtime,
                warning: None,
            }),
            Err(error) => match selected {
                Ok(runtime) => {
                    let warning = format!(
                        "安装建议 Java 失败，将尝试本机 Java {}：{error:#}",
                        runtime.major()
                    );
                    Ok(RuntimeResolution {
                        runtime,
                        warning: Some(warning),
                    })
                }
                Err(local_error) => Err(error).with_context(|| format!("本机没有满足要求的 Java（{local_error:#}），且安装建议 Java {required} 失败")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Source {
        local: Option<u64>,
        installed: Option<u64>,
        calls: Cell<usize>,
    }
    fn runtime(major: u64) -> JavaRuntime {
        JavaRuntime {
            path: format!("java{major}").into(),
            version: crate::java_version::JavaVersion::parse(&major.to_string()).unwrap(),
            description: String::new(),
        }
    }
    impl JavaRuntimeSource for Source {
        fn select(&self, _config: &Config, _required: u64) -> Result<JavaRuntime> {
            self.local
                .map(runtime)
                .ok_or_else(|| anyhow::anyhow!("未安装 Java"))
        }
        fn install(
            &self,
            _config: &Config,
            _instance: &str,
            _required: u64,
        ) -> Result<JavaRuntime> {
            self.calls.set(self.calls.get() + 1);
            self.installed
                .map(runtime)
                .ok_or_else(|| anyhow::anyhow!("下载失败"))
        }
    }
    #[test]
    fn cancellation_during_install_does_not_turn_into_a_successful_fallback() {
        struct CancellingSource(Cell<bool>);
        impl JavaRuntimeSource for CancellingSource {
            fn check_task(&self) -> Result<()> {
                if self.0.get() {
                    bail!("任务已取消");
                }
                Ok(())
            }
            fn select(&self, _config: &Config, _required: u64) -> Result<JavaRuntime> {
                Ok(runtime(25))
            }
            fn install(
                &self,
                _config: &Config,
                _instance: &str,
                _required: u64,
            ) -> Result<JavaRuntime> {
                self.0.set(true);
                bail!("下载已取消");
            }
        }
        let source = CancellingSource(Cell::new(false));
        let result = JavaRuntimeService::new(&source).resolve(
            &Config::default(),
            "game",
            JavaRequirement::minimum(21),
        );
        assert_eq!(result.err().unwrap().to_string(), "任务已取消");
    }
    #[test]
    fn exact_local_and_explicit_paths_never_trigger_installation() {
        let source = Source {
            local: Some(21),
            installed: None,
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        assert_eq!(
            service
                .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
                .unwrap()
                .into_runtime()
                .major(),
            21
        );
        let explicit = Config {
            java: "custom-java".into(),
            ..Config::default()
        };
        assert!(service
            .resolve(&explicit, "game", JavaRequirement::minimum(25))
            .is_err());
        assert_eq!(source.calls.get(), 0);
    }
    #[test]
    fn auto_prefers_recommended_install_and_reports_a_safe_fallback() {
        let source = Source {
            local: Some(25),
            installed: Some(21),
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        assert_eq!(
            service
                .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
                .unwrap()
                .into_runtime()
                .major(),
            21
        );
        let failing = Source {
            installed: None,
            ..source
        };
        let resolution = JavaRuntimeService::new(&failing)
            .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
            .unwrap();
        assert!(resolution.warning().unwrap().contains("下载失败"));
        assert_eq!(resolution.into_runtime().major(), 25);
    }
    #[test]
    fn cannot_fallback_to_too_old_java_or_accept_wrong_installed_version() {
        for local in [None, Some(8)] {
            let source = Source {
                local,
                installed: Some(17),
                calls: Cell::new(0),
            };
            let error = JavaRuntimeService::new(&source)
                .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("预期 21"));
            assert_eq!(source.calls.get(), 1);
        }
    }
    fn wrapper(version: &str) -> Value {
        serde_json::json!({
            "mainClass":"net.minecraft.launchwrapper.Launch",
            "libraries":[{"name":format!("net.minecraft:launchwrapper:{version}")}],
            "javaVersion":{"majorVersion":8}
        })
    }
    #[test]
    fn old_wrapper_cannot_fallback_to_new_java_or_silently_replace_explicit_java() {
        let source = Source {
            local: Some(21),
            installed: None,
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        assert!(service
            .resolve(
                &Config::default(),
                "renamed-instance",
                JavaRequirement::from_metadata(&wrapper("1.12")).unwrap()
            )
            .is_err());
        assert_eq!(source.calls.get(), 1);
        let explicit = Config {
            java: "custom-java".into(),
            ..Default::default()
        };
        let error = service
            .resolve(
                &explicit,
                "renamed-instance",
                JavaRequirement::from_metadata(&wrapper("1.12")).unwrap(),
            )
            .err()
            .unwrap();
        assert!(error
            .to_string()
            .contains("旧 LaunchWrapper 不兼容 Java 21"));
        assert_eq!(source.calls.get(), 1);
    }
    #[test]
    fn old_wrapper_reuses_or_installs_compatible_java_eight() {
        for local in [Some(8), Some(21), None] {
            let source = Source {
                local,
                installed: Some(8),
                calls: Cell::new(0),
            };
            let runtime = JavaRuntimeService::new(&source)
                .resolve(
                    &Config::default(),
                    "game",
                    JavaRequirement::from_metadata(&wrapper("1.12")).unwrap(),
                )
                .unwrap()
                .into_runtime();
            assert_eq!(runtime.major(), 8);
            assert_eq!(source.calls.get(), usize::from(local != Some(8)));
        }
    }
    #[test]
    fn newer_wrapper_or_different_launcher_does_not_inherit_legacy_restriction() {
        for mut meta in [wrapper("1.13"), wrapper("2.0"), wrapper("1.12")] {
            if meta["libraries"][0]["name"] == "net.minecraft:launchwrapper:1.12" {
                meta["mainClass"] = "net.minecraft.client.main.Main".into();
            }
            assert!(JavaRequirement::from_metadata(&meta)
                .unwrap()
                .check(runtime(21))
                .is_ok());
        }
    }
    #[test]
    fn conflicting_or_invalid_java_metadata_is_rejected_before_runtime_selection() {
        for major in [
            serde_json::json!(17),
            serde_json::json!(0),
            serde_json::json!("8"),
            Value::Null,
        ] {
            let mut meta = wrapper("1.12");
            meta["javaVersion"]["majorVersion"] = major;
            assert!(JavaRequirement::from_metadata(&meta).is_err());
        }
        assert_eq!(
            JavaRequirement::from_metadata(&serde_json::json!({}))
                .unwrap()
                .recommended(),
            8
        );
    }
    fn fabric(version: &str) -> Value {
        serde_json::json!({"mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient", "javaVersion":{"majorVersion":21}, "libraries":[{"name":format!("net.fabricmc:fabric-loader:{version}")}]})
    }
    #[test]
    fn old_fabric_rejects_java_25_in_explicit_and_failed_download_fallback_modes() {
        let source = Source {
            local: Some(25),
            installed: None,
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        let error = service
            .resolve(
                &Config::default(),
                "renamed-fabric",
                JavaRequirement::from_metadata(&fabric("0.16.14")).unwrap(),
            )
            .err()
            .unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("Fabric Loader 0.17.0 之前的版本 不兼容 Java 25"));
        assert!(message.contains("下载失败"));
        assert_eq!(source.calls.get(), 1);
        let explicit = Config {
            java: "custom-java".into(),
            ..Default::default()
        };
        assert!(service
            .resolve(
                &explicit,
                "renamed-fabric",
                JavaRequirement::from_metadata(&fabric("0.16.14")).unwrap()
            )
            .is_err());
        assert_eq!(source.calls.get(), 1);
    }
    #[test]
    fn old_fabric_allows_compatible_fallback_and_recommended_installation() {
        for installed in [None, Some(21)] {
            let source = Source {
                local: Some(24),
                installed,
                calls: Cell::new(0),
            };
            let resolution = JavaRuntimeService::new(&source)
                .resolve(
                    &Config::default(),
                    "fabric",
                    JavaRequirement::from_metadata(&fabric("0.16.14")).unwrap(),
                )
                .unwrap();
            assert_eq!(resolution.warning().is_some(), installed.is_none());
            assert_eq!(resolution.into_runtime().major(), installed.unwrap_or(24));
        }
    }
    #[test]
    fn fabric_limits_follow_active_library_and_launcher_not_the_instance_label() {
        for (version, restricted) in [
            ("0.16.14", true),
            ("0.17.0", false),
            ("0.18.0", false),
            ("1.0.0", false),
            ("0.17.0-beta", false),
        ] {
            assert_eq!(
                JavaRequirement::from_metadata(&fabric(version))
                    .unwrap()
                    .check(runtime(25))
                    .is_err(),
                restricted
            );
        }
        let mut inactive = fabric("0.16.14");
        inactive["libraries"][0]["rules"] = serde_json::json!([{"action":"disallow"}]);
        assert!(JavaRequirement::from_metadata(&inactive)
            .unwrap()
            .check(runtime(25))
            .is_ok());
        inactive["libraries"][0]
            .as_object_mut()
            .unwrap()
            .remove("rules");
        inactive["mainClass"] = "net.minecraft.client.main.Main".into();
        assert!(JavaRequirement::from_metadata(&inactive)
            .unwrap()
            .check(runtime(25))
            .is_ok());
        let mut conflict = fabric("0.16.14");
        conflict["javaVersion"]["majorVersion"] = 25.into();
        assert!(JavaRequirement::from_metadata(&conflict).is_err());
    }
}
