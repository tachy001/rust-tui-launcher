use anyhow::{bail, Result};
use serde_json::Value;

pub(crate) struct JvmArgumentCompatibility {
    major: u64,
}
pub(crate) struct CompatibleJvmArguments {
    pub(crate) arguments: Vec<String>,
    pub(crate) warnings: Vec<String>,
}
impl JvmArgumentCompatibility {
    pub(crate) fn new(major: u64) -> Self {
        Self { major }
    }
    pub(crate) fn normalize(&self, arguments: &[String]) -> Result<CompatibleJvmArguments> {
        let mut output = CompatibleJvmArguments {
            arguments: vec![],
            warnings: vec![],
        };
        let mut removed = std::collections::BTreeSet::new();
        let mut collectors = std::collections::BTreeMap::new();
        for argument in arguments {
            let xx = argument.strip_prefix("-XX:");
            let name = xx.map(|value| {
                value
                    .trim_start_matches(['+', '-'])
                    .split('=')
                    .next()
                    .unwrap_or(value)
            });
            let obsolete = if self.major >= 9 && argument == "-Xincgc" {
                Some("-Xincgc")
            } else if (self.major >= 9
                && name.is_some_and(|name| {
                    matches!(
                        name,
                        "CMSIncrementalMode"
                            | "UseCMSCompactAtFullCollection"
                            | "CMSFullGCsBeforeCompaction"
                            | "UseCMSCollectionPassing"
                    )
                }))
                || (self.major >= 10
                    && name.is_some_and(|name| matches!(name, "PermSize" | "MaxPermSize")))
            {
                name
            } else {
                None
            };
            if let Some(name) = obsolete {
                if removed.insert(name.to_owned()) {
                    output.warnings.push(format!(
                        "已忽略 Java {} 不再支持的 JVM 参数 {name}；原配置未修改",
                        self.major
                    ));
                }
                continue;
            }
            if let Some((enabled, name)) = xx.and_then(|value| {
                value
                    .strip_prefix('+')
                    .map(|name| (true, name))
                    .or_else(|| value.strip_prefix('-').map(|name| (false, name)))
            }) {
                if matches!(
                    name,
                    "UseSerialGC"
                        | "UseParallelGC"
                        | "UseG1GC"
                        | "UseZGC"
                        | "UseShenandoahGC"
                        | "UseEpsilonGC"
                        | "UseConcMarkSweepGC"
                ) {
                    // CMS is ignored by JDK 14 and absent thereafter. Its flags
                    // are left for the JVM, not treated as a selected collector.
                    if name != "UseConcMarkSweepGC" || self.major < 14 {
                        collectors.insert(name, enabled);
                    }
                }
            }
            output.arguments.push(argument.clone());
        }
        let enabled: Vec<_> = collectors
            .into_iter()
            .filter_map(|(name, enabled)| enabled.then_some(name))
            .collect();
        if enabled.len() > 1 {
            bail!("同时启用了多个 JVM 垃圾回收器：{}；请在自定义 JVM 参数中只保留一个，或显式关闭不使用的选项", enabled.join("、"));
        }
        Ok(output)
    }
}

/// Defaults are evaluated after metadata and user arguments, before expansion.
/// Explicit properties (including an empty value) retain their existing meaning.
pub(crate) struct JvmArgumentDefaults<'a> {
    major: u64,
    os: &'a str,
}
impl<'a> JvmArgumentDefaults<'a> {
    pub(crate) fn new(major: u64, os: &'a str) -> Self {
        Self { major, os }
    }

    pub(crate) fn apply(&self, arguments: &mut Vec<String>, metadata: &Value) {
        for (name, value) in [
            ("java.rmi.server.useCodebaseOnly", "true"),
            ("com.sun.jndi.rmi.object.trustURLCodebase", "false"),
            ("com.sun.jndi.cosnaming.object.trustURLCodebase", "false"),
            ("log4j2.formatMsgNoLookups", "true"),
            ("minecraft.client.jar", "${primary_jar}"),
        ] {
            Self::property(arguments, name, value);
        }
        // Keep file.encoding unchanged: legacy games/mods may depend on the
        // JVM's native charset. Only standard output is read as UTF-8 by us.
        for name in if self.major < 19 {
            ["sun.stdout.encoding", "sun.stderr.encoding"]
        } else {
            ["stdout.encoding", "stderr.encoding"]
        } {
            Self::property(arguments, name, "UTF-8");
        }
        let lwjgl3 = metadata["libraries"].as_array().is_some_and(|libraries| {
            libraries.iter().any(|library| {
                library["name"].as_str().is_some_and(|name| {
                    name.starts_with("org.lwjgl:lwjgl:3.")
                        || name.starts_with("org.lwjgl:lwjgl-glfw:")
                })
            })
        });
        if self.os == "macos"
            && lwjgl3
            && !arguments
                .iter()
                .any(|argument| argument == "-XstartOnFirstThread")
        {
            arguments.push("-XstartOnFirstThread".into());
        }
    }

    fn property(arguments: &mut Vec<String>, name: &str, value: &str) {
        let property = format!("-D{name}");
        if !arguments.iter().any(|argument| {
            argument == &property
                || argument
                    .strip_prefix(&property)
                    .is_some_and(|suffix| suffix.starts_with('='))
        }) {
            arguments.push(format!("{property}={value}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options(arguments: &[&str]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| (*argument).into())
            .collect()
    }

    #[test]
    fn expired_options_follow_actual_java_boundaries_and_never_rewrite_config() {
        let args = options(&[
            "-Xincgc",
            "-XX:+CMSIncrementalMode",
            "-XX:CMSFullGCsBeforeCompaction=5",
            "-XX:PermSize=64M",
            "-XX:MaxPermSize=256M",
            "-Dexample=-Xincgc",
            "-Xmx1G",
        ]);
        assert_eq!(
            JvmArgumentCompatibility::new(8)
                .normalize(&args)
                .unwrap()
                .arguments,
            args
        );
        let java9 = JvmArgumentCompatibility::new(9).normalize(&args).unwrap();
        assert!(java9.arguments.contains(&"-XX:MaxPermSize=256M".into()));
        assert!(!java9.arguments.contains(&"-Xincgc".into()));
        let java10 = JvmArgumentCompatibility::new(10).normalize(&args).unwrap();
        assert_eq!(java10.arguments, ["-Dexample=-Xincgc", "-Xmx1G"]);
        assert_eq!(java10.warnings.len(), 5);
        assert_eq!(args[0], "-Xincgc");
        let repeated = options(&["-Xincgc", "-Xincgc"]);
        assert_eq!(
            JvmArgumentCompatibility::new(21)
                .normalize(&repeated)
                .unwrap()
                .warnings
                .len(),
            1
        );
    }

    #[test]
    fn collector_conflicts_respect_last_boolean_setting_without_guessing_vendor_support() {
        let policy = JvmArgumentCompatibility::new(21);
        assert!(policy
            .normalize(&options(&["-XX:+UseG1GC", "-XX:+UseZGC"]))
            .is_err());
        assert!(policy
            .normalize(&options(&["-XX:+UseG1GC", "-XX:+UseZGC", "-XX:-UseG1GC"]))
            .is_ok());
        assert!(policy
            .normalize(&options(&["-XX:+UseG1GC", "-XX:+UseG1GC"]))
            .is_ok());
        assert!(policy
            .normalize(&options(&["-XX:+UseShenandoahGC", "-XX:+VendorOption"]))
            .is_ok());
        let cms = options(&["-XX:+UseConcMarkSweepGC", "-XX:+UseG1GC"]);
        assert!(JvmArgumentCompatibility::new(13).normalize(&cms).is_err());
        assert!(JvmArgumentCompatibility::new(14).normalize(&cms).is_ok());
        assert!(policy
            .normalize(&options(&["-Dexample=-XX:+UseZGC", "-XX:+UseG1GC"]))
            .is_ok());
    }

    #[test]
    fn stdout_property_names_follow_java_boundary_without_changing_file_encoding() {
        for major in [8, 17, 18, 19, 21, 25] {
            let mut args = vec!["-Dfile.encoding=GBK".into()];
            let defaults = JvmArgumentDefaults::new(major, "windows");
            defaults.apply(&mut args, &json!({}));
            let stdout = if major < 19 {
                "sun.stdout.encoding"
            } else {
                "stdout.encoding"
            };
            let stderr = if major < 19 {
                "sun.stderr.encoding"
            } else {
                "stderr.encoding"
            };
            assert!(args.contains(&format!("-D{stdout}=UTF-8")));
            assert!(args.contains(&format!("-D{stderr}=UTF-8")));
            assert_eq!(
                args.iter()
                    .filter(|arg| arg.starts_with("-Dfile.encoding"))
                    .collect::<Vec<_>>(),
                [&"-Dfile.encoding=GBK".to_owned()]
            );
            let first = args.clone();
            defaults.apply(&mut args, &json!({}));
            assert_eq!(args, first);
        }
    }

    #[test]
    fn explicit_empty_and_custom_properties_are_preserved_and_similar_names_do_not_hide_defaults() {
        let mut args = vec![
            "-Dlog4j2.formatMsgNoLookups=false".into(),
            "-Djava.rmi.server.useCodebaseOnly".into(),
            "-Dsun.stdout.encoding=".into(),
            "-Dminecraft.client.jar=/custom.jar".into(),
            "-Dsun.stderr.encoding.extra=GBK".into(),
        ];
        let first = args.clone();
        JvmArgumentDefaults::new(8, "linux").apply(&mut args, &json!({}));
        assert_eq!(&args[..first.len()], &first);
        assert!(!args.contains(&"-Dlog4j2.formatMsgNoLookups=true".into()));
        assert!(!args.contains(&"-Dsun.stdout.encoding=UTF-8".into()));
        assert!(args.contains(&"-Dsun.stderr.encoding=UTF-8".into()));
        assert!(!args.contains(&"-Dminecraft.client.jar=${primary_jar}".into()));
    }

    #[test]
    fn defaults_expand_client_path_once_and_keep_platform_thread_rule() {
        let mut args = vec![];
        let meta = json!({"libraries":[{"name":"org.lwjgl:lwjgl:3.3.3"}]});
        let defaults = JvmArgumentDefaults::new(21, "macos");
        defaults.apply(&mut args, &meta);
        defaults.apply(&mut args, &meta);
        assert_eq!(
            args.iter()
                .filter(|arg| *arg == "-XstartOnFirstThread")
                .count(),
            1
        );
        let template = args
            .iter()
            .find(|arg| arg.starts_with("-Dminecraft.client.jar="))
            .unwrap();
        let path = "game 中文 ${profile}/client.jar";
        let values = std::collections::HashMap::from([("primary_jar", path.into())]);
        assert_eq!(
            crate::launch_options::expand_argument(template, &values).unwrap(),
            format!("-Dminecraft.client.jar={path}")
        );
        for (os, metadata) in [
            ("linux", meta),
            (
                "macos",
                json!({"libraries":[{"name":"org.lwjgl.lwjgl:lwjgl:2.9.1"}]}),
            ),
        ] {
            let mut args = vec![];
            JvmArgumentDefaults::new(8, os).apply(&mut args, &metadata);
            assert!(!args.contains(&"-XstartOnFirstThread".into()));
        }
    }
}
