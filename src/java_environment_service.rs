use std::{ffi::OsStr, process::Command};

const OPTION_VARIABLES: [&str; 3] = ["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS"];

/// Keep the launcher's validated JVM argv authoritative without changing the
/// host environment or discarding display, locale, PATH, or graphics settings.
pub(crate) struct JavaEnvironment;
impl JavaEnvironment {
    pub(crate) fn apply(&self, command: &mut Command) -> Vec<&'static str> {
        let mut removed = vec![];
        for name in OPTION_VARIABLES {
            let explicit = command.get_envs().find(|(key, _)| {
                if cfg!(windows) {
                    key.to_str()
                        .is_some_and(|key| key.eq_ignore_ascii_case(name))
                } else {
                    *key == OsStr::new(name)
                }
            });
            let present = match explicit {
                Some((_, value)) => value.is_some_and(|value| !value.is_empty()),
                None => std::env::var_os(name).is_some_and(|value| !value.is_empty()),
            };
            if present {
                removed.push(name);
            }
            command.env_remove(name);
        }
        removed
    }
    pub(crate) fn notice(&self, removed: &[&str]) -> Option<String> {
        (!removed.is_empty()).then(|| format!(
            "本次 Java 进程已忽略外部选项环境变量：{}；如需这些选项，请在启动器 JVM 参数中明确配置。系统环境未修改。",
            removed.join("、")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_java_option_injection_is_removed_and_values_are_never_reported() {
        let mut command = Command::new("java");
        command
            .env("JAVA_TOOL_OPTIONS", "-Dpassword=private-value")
            .env("_JAVA_OPTIONS", "-Xmx8G")
            .env("JDK_JAVA_OPTIONS", "@external.args")
            .env("DISPLAY", ":fixture")
            .env("LANG", "zh_CN.UTF-8")
            .env("PATH", "fixture-path")
            .env("LD_LIBRARY_PATH", "fixture-native-path");
        let removed = JavaEnvironment.apply(&mut command);
        assert_eq!(removed, OPTION_VARIABLES);
        let notice = JavaEnvironment.notice(&removed).unwrap();
        assert!(notice.contains("JAVA_TOOL_OPTIONS"));
        assert!(!notice.contains("private-value"));
        assert!(!notice.contains("8G"));
        let environment: std::collections::HashMap<_, _> = command.get_envs().collect();
        for key in OPTION_VARIABLES {
            assert_eq!(environment[OsStr::new(key)], None);
        }
        assert_eq!(
            environment[OsStr::new("DISPLAY")],
            Some(OsStr::new(":fixture"))
        );
        assert_eq!(
            environment[OsStr::new("LANG")],
            Some(OsStr::new("zh_CN.UTF-8"))
        );
        assert_eq!(
            environment[OsStr::new("PATH")],
            Some(OsStr::new("fixture-path"))
        );
        assert_eq!(
            environment[OsStr::new("LD_LIBRARY_PATH")],
            Some(OsStr::new("fixture-native-path"))
        );
        assert!(JavaEnvironment.apply(&mut command).is_empty());
        assert!(JavaEnvironment.notice(&[]).is_none());
    }
    #[test]
    fn actual_child_receives_filtered_environment_without_global_mutation() {
        let before: Vec<_> = OPTION_VARIABLES.iter().map(std::env::var_os).collect();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "java_environment_service::tests::child_environment_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("TUI_ENV_PROBE", "yes")
            .env("TUI_ENV_PRESERVED", "keep");
        for key in OPTION_VARIABLES {
            command.env(key, "invalid-external-option");
        }
        JavaEnvironment.apply(&mut command);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("filtered-child-ok"));
        assert_eq!(
            before,
            OPTION_VARIABLES
                .iter()
                .map(std::env::var_os)
                .collect::<Vec<_>>()
        );
    }
    #[test]
    #[ignore = "subprocess helper; no Java execution"]
    fn child_environment_probe() {
        if std::env::var("TUI_ENV_PROBE").as_deref() != Ok("yes") {
            return;
        }
        for key in OPTION_VARIABLES {
            assert!(std::env::var_os(key).is_none());
        }
        assert_eq!(std::env::var("TUI_ENV_PRESERVED").unwrap(), "keep");
        println!("filtered-child-ok");
    }
}
