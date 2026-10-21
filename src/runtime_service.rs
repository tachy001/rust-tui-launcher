use crate::{backend::Backend, config::Config, java::JavaRuntime};
use anyhow::{bail, Result};

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
        required: u64,
    ) -> Result<RuntimeResolution> {
        self.source.check_task()?;
        let selected = self.source.select(config, required).and_then(|runtime| {
            if runtime.major < required {
                bail!("需要 Java {required}，当前 Java {}", runtime.major);
            }
            Ok(runtime)
        });
        self.source.check_task()?;
        if config.java != "auto"
            || selected
                .as_ref()
                .is_ok_and(|runtime| runtime.major == required)
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
                if runtime.major != required {
                    bail!(
                        "下载后的 Java 版本不符：预期 {required}，实际 {}",
                        runtime.major
                    );
                }
                Ok(runtime)
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
                        runtime.major
                    );
                    Ok(RuntimeResolution {
                        runtime,
                        warning: Some(warning),
                    })
                }
                Err(_) => Err(error),
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
            major,
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
        let result = JavaRuntimeService::new(&source).resolve(&Config::default(), "game", 21);
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
                .resolve(&Config::default(), "game", 21)
                .unwrap()
                .into_runtime()
                .major,
            21
        );
        let explicit = Config {
            java: "custom-java".into(),
            ..Config::default()
        };
        assert!(service.resolve(&explicit, "game", 25).is_err());
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
                .resolve(&Config::default(), "game", 21)
                .unwrap()
                .into_runtime()
                .major,
            21
        );
        let failing = Source {
            installed: None,
            ..source
        };
        let resolution = JavaRuntimeService::new(&failing)
            .resolve(&Config::default(), "game", 21)
            .unwrap();
        assert!(resolution.warning().unwrap().contains("下载失败"));
        assert_eq!(resolution.into_runtime().major, 25);
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
                .resolve(&Config::default(), "game", 21)
                .err()
                .unwrap();
            assert!(error.to_string().contains("预期 21"));
            assert_eq!(source.calls.get(), 1);
        }
    }
}
