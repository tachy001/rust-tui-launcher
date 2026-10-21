use regex::Regex;
use serde_json::Value;
use std::{collections::HashMap, sync::OnceLock};

pub(crate) trait OsVersionSource {
    fn version(&self) -> Option<&str>;
}
struct HostVersion;
impl OsVersionSource for HostVersion {
    fn version(&self) -> Option<&str> {
        static VERSION: OnceLock<Option<String>> = OnceLock::new();
        VERSION
            .get_or_init(|| {
                let output = if cfg!(windows) {
                    std::process::Command::new("cmd")
                        .args(["/C", "ver"])
                        .output()
                } else if cfg!(target_os = "macos") {
                    std::process::Command::new("sw_vers")
                        .arg("-productVersion")
                        .output()
                } else {
                    std::process::Command::new("uname").arg("-r").output()
                }
                .ok()?;
                if !output.status.success() {
                    return None;
                }
                Regex::new(r"\d+\.\d+(?:\.\d+)*")
                    .ok()?
                    .find(&String::from_utf8_lossy(&output.stdout))
                    .map(|value| value.as_str().to_owned())
            })
            .as_deref()
    }
}

/// One evaluator can target a different platform without executing platform commands.
pub(crate) struct PlatformRuleEvaluator<'a> {
    os: &'a str,
    arch: &'a str,
    version: &'a dyn OsVersionSource,
}
impl<'a> PlatformRuleEvaluator<'a> {
    pub(crate) fn new(os: &'a str, arch: &'a str, version: &'a dyn OsVersionSource) -> Self {
        Self { os, arch, version }
    }
    pub(crate) fn current() -> PlatformRuleEvaluator<'static> {
        PlatformRuleEvaluator::new(
            crate::metadata::os_name(),
            std::env::consts::ARCH,
            &HostVersion,
        )
    }
    fn aliases(&self) -> &[&str] {
        match self.arch {
            "x86_64" | "amd64" | "x64" | "x86-64" => &["x86_64", "amd64", "x64", "x86-64"],
            "aarch64" | "arm64" => &["aarch64", "arm64"],
            "x86" | "i386" | "i686" => &["x86", "i386", "i686"],
            _ => std::slice::from_ref(&self.arch),
        }
    }
    fn matches_name(&self, name: &str) -> bool {
        if name == "unknown" {
            return true;
        }
        let (os, arch) = name
            .split_once('-')
            .map_or((name, None), |(os, arch)| (os, Some(arch)));
        let os = match os {
            "win" => "windows",
            "macos" | "darwin" => "osx",
            os => os,
        };
        os == self.os && arch.is_none_or(|arch| self.aliases().contains(&arch))
    }
    fn matches_os(&self, os: &Value) -> bool {
        if os.is_null() {
            return true;
        }
        if !os.is_object() {
            return false;
        }
        if let Some(name) = os.get("name") {
            if !name.as_str().is_some_and(|name| self.matches_name(name)) {
                return false;
            }
        }
        if let Some(pattern) = os.get("arch") {
            let Some(pattern) = pattern.as_str() else {
                return false;
            };
            let Ok(regex) = Regex::new(&format!(r"\A(?:{pattern})\z")) else {
                return false;
            };
            if !self.aliases().iter().any(|arch| regex.is_match(arch)) {
                return false;
            }
        }
        if let Some(pattern) = os.get("version") {
            let Some(pattern) = pattern.as_str() else {
                return false;
            };
            let Ok(regex) = Regex::new(pattern) else {
                return false;
            };
            // Keep prefix/search matching used by existing manifests and PCL.
            if !self
                .version
                .version()
                .is_some_and(|version| regex.is_match(version))
            {
                return false;
            }
        }
        true
    }
    pub(crate) fn allowed(&self, item: &Value, features: &HashMap<&str, bool>) -> bool {
        let rules = match item.get("rules") {
            None | Some(Value::Null) => return true,
            Some(Value::Array(rules)) if rules.is_empty() => return true,
            Some(Value::Array(rules)) => rules,
            _ => return false,
        };
        let mut allowed = false;
        for rule in rules {
            let action = match rule["action"].as_str() {
                Some("allow") => true,
                Some("disallow") => false,
                _ => continue,
            };
            if rule.get("os").is_some_and(|os| !self.matches_os(os)) {
                continue;
            }
            if let Some(required) = rule.get("features").filter(|value| !value.is_null()) {
                let Some(required) = required.as_object() else {
                    continue;
                };
                if required.iter().any(|(name, expected)| {
                    expected.as_bool()
                        != Some(features.get(name.as_str()).copied().unwrap_or(false))
                }) {
                    continue;
                }
            }
            allowed = action;
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Version(Option<&'static str>);
    impl OsVersionSource for Version {
        fn version(&self) -> Option<&str> {
            self.0
        }
    }
    fn item(os: Value) -> Value {
        serde_json::json!({"rules":[{"action":"allow","os":os}]})
    }
    #[test]
    fn architecture_regex_and_aliases_match_without_confusing_x86_and_x64() {
        let version = Version(None);
        let windows = PlatformRuleEvaluator::new("windows", "x86_64", &version);
        for arch in ["x86_64", "amd64", "x64", "x86_64|aarch64"] {
            assert!(
                windows.allowed(&item(serde_json::json!({"arch":arch})), &HashMap::new()),
                "{arch}"
            );
        }
        assert!(!windows.allowed(&item(serde_json::json!({"arch":"x86"})), &HashMap::new()));
        let mac = PlatformRuleEvaluator::new("osx", "aarch64", &version);
        assert!(mac.allowed(
            &item(serde_json::json!({"name":"macos-arm64","arch":"arm64|aarch64"})),
            &HashMap::new()
        ));
        assert!(!mac.allowed(
            &item(serde_json::json!({"name":"windows"})),
            &HashMap::new()
        ));
    }
    #[test]
    fn platform_version_keeps_prefix_patterns_and_unknown_version_does_not_match() {
        let version = Version(Some("10.0.22631"));
        let windows = PlatformRuleEvaluator::new("windows", "x86_64", &version);
        assert!(windows.allowed(
            &item(serde_json::json!({"name":"windows","version":"^10\\."})),
            &HashMap::new()
        ));
        let unknown = Version(None);
        let linux = PlatformRuleEvaluator::new("linux", "x86_64", &unknown);
        assert!(!linux.allowed(&item(serde_json::json!({"version":".*"})), &HashMap::new()));
    }
    #[test]
    fn rule_order_and_features_apply_to_every_target_platform() {
        let version = Version(Some("6.8.0"));
        for (os, arch) in [
            ("windows", "x86_64"),
            ("linux", "x86_64"),
            ("osx", "aarch64"),
        ] {
            let evaluator = PlatformRuleEvaluator::new(os, arch, &version);
            let item = serde_json::json!({"rules":[{"action":"allow"},{"action":"disallow","features":{"blocked":true}},{"action":"invalid"}]});
            assert!(evaluator.allowed(&item, &HashMap::new()));
            assert!(!evaluator.allowed(&item, &HashMap::from([("blocked", true)])));
            assert!(evaluator.allowed(&serde_json::json!({"rules":[]}), &HashMap::new()));
            assert!(evaluator.allowed(
                &serde_json::json!({"rules":[{"action":"allow","os":null,"features":null}]}),
                &HashMap::new()
            ));
        }
    }
    #[test]
    fn malformed_architecture_and_version_rules_do_not_match() {
        let version = Version(Some("10.0"));
        let evaluator = PlatformRuleEvaluator::new("windows", "x86_64", &version);
        for os in [
            serde_json::json!({"arch":"["}),
            serde_json::json!({"version":"["}),
            serde_json::json!({"arch":42}),
            serde_json::json!({"name":"unrecognized"}),
        ] {
            assert!(!evaluator.allowed(&item(os), &HashMap::new()));
        }
        assert!(!evaluator.allowed(&serde_json::json!({"rules":"allow"}), &HashMap::new()));
    }
    #[test]
    fn version_provider_is_not_queried_for_rules_that_do_not_require_it() {
        struct Never;
        impl OsVersionSource for Never {
            fn version(&self) -> Option<&str> {
                panic!("unexpected platform probe")
            }
        }
        let evaluator = PlatformRuleEvaluator::new("linux", "x86_64", &Never);
        assert!(evaluator.allowed(
            &item(serde_json::json!({"name":"linux","arch":"amd64"})),
            &HashMap::new()
        ));
        assert!(!evaluator.allowed(
            &item(serde_json::json!({"name":"osx","version":".*"})),
            &HashMap::new()
        ));
    }
}
