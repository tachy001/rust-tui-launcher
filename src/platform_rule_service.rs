use regex::Regex;
use serde_json::Value;
use std::{
    collections::HashMap,
    process::{Command, Stdio},
    sync::OnceLock,
};

pub(crate) trait OsVersionSource {
    fn version(&self) -> Option<&str>;
}
struct HostVersion;
impl OsVersionSource for HostVersion {
    fn version(&self) -> Option<&str> {
        static VERSION: OnceLock<Option<String>> = OnceLock::new();
        VERSION
            .get_or_init(|| {
                let mut command = if cfg!(windows) {
                    let mut command = Command::new("cmd");
                    // AutoRun may print unrelated version numbers or wait
                    // for input before the built-in `ver` command executes.
                    command.args(["/D", "/C", "ver"]);
                    command
                } else if cfg!(target_os = "macos") {
                    let mut command = Command::new("sw_vers");
                    command.arg("-productVersion");
                    command
                } else {
                    let mut command = Command::new("uname");
                    command.arg("-r");
                    command
                };
                let output = VersionProbe {
                    timeout: std::time::Duration::from_secs(5),
                }
                .run(&mut command)
                .ok()?;
                parse_host_version(&String::from_utf8_lossy(&output), cfg!(windows))
            })
            .as_deref()
    }
}

struct VersionProbe {
    timeout: std::time::Duration,
}
impl VersionProbe {
    fn run(&self, command: &mut Command) -> anyhow::Result<Vec<u8>> {
        use crate::process_service::{ManagedProcess, ProcessHandle};
        use anyhow::{bail, Context};
        use std::{
            io::Read,
            sync::mpsc,
            thread,
            time::{Duration, Instant},
        };
        const LIMIT: u64 = 4096;
        let mut child = ManagedProcess::spawn(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )?;
        let stdout = child.take_stdout().context("系统版本探测没有输出管道")?;
        let (sender, receiver) = mpsc::sync_channel(1);
        // Read at most LIMIT + 1 bytes; never join a reader that may still be
        // waiting on an inherited pipe after a process cleanup failure.
        thread::Builder::new()
            .name("os-version-output".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout
                    .take(LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = sender.send(result);
            })?;
        let started = Instant::now();
        let result = (|| {
            let mut output = None;
            loop {
                if output.is_none() {
                    match receiver.try_recv() {
                        Ok(bytes) => {
                            let bytes = bytes.context("读取系统版本失败")?;
                            if bytes.len() as u64 > LIMIT {
                                bail!("系统版本探测输出过长");
                            }
                            output = Some(bytes);
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                        Err(mpsc::TryRecvError::Disconnected) => bail!("系统版本读取线程意外退出"),
                    }
                }
                if let Some(status) = child.poll()? {
                    if !status.success() {
                        bail!("系统版本探测失败：{status}");
                    }
                    if let Some(bytes) = output {
                        return Ok(bytes);
                    }
                }
                if started.elapsed() >= self.timeout {
                    bail!("系统版本探测超时");
                }
                thread::sleep(Duration::from_millis(10));
            }
        })();
        child.terminate().context("系统版本探测进程清理失败")?;
        result
    }
}

fn parse_host_version(output: &str, windows: bool) -> Option<String> {
    if windows {
        Regex::new(r"\d+\.\d+(?:\.\d+)*")
            .ok()?
            .find(output)
            .map(|value| value.as_str().to_owned())
    } else {
        let version = output.trim();
        (!version.is_empty()).then(|| version.to_owned())
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
    pub(crate) fn for_java(arch: &'static str) -> PlatformRuleEvaluator<'static> {
        PlatformRuleEvaluator::new(crate::metadata::os_name(), arch, &HostVersion)
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
    #[test]
    #[ignore = "subprocess fixture invoked by version_probe_bounds_output_and_lifetime"]
    fn version_probe_child() {
        match std::env::var("TUI_VERSION_PROBE_FIXTURE").unwrap().as_str() {
            "hang" => std::thread::sleep(std::time::Duration::from_secs(30)),
            "large" => println!("{}", "x".repeat(8192)),
            "fail" => std::process::exit(23),
            _ => println!("6.8.0-test-kernel"),
        }
    }
    #[test]
    fn version_probe_bounds_output_and_lifetime() {
        for (mode, expected) in [
            ("ok", ""),
            ("large", "输出过长"),
            ("fail", "探测失败"),
            ("hang", "超时"),
        ] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "platform_rule_service::tests::version_probe_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("TUI_VERSION_PROBE_FIXTURE", mode);
            let probe = VersionProbe {
                timeout: std::time::Duration::from_secs(2),
            };
            let started = std::time::Instant::now();
            let result = probe.run(&mut command);
            if mode == "ok" {
                assert!(String::from_utf8(result.unwrap())
                    .unwrap()
                    .contains("6.8.0-test-kernel"));
            } else {
                assert!(
                    format!("{:#}", result.unwrap_err()).contains(expected),
                    "{mode}"
                );
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(15));
        }
    }
    #[test]
    fn host_version_preserves_kernel_suffix_for_version_rules() {
        let parsed = parse_host_version("6.8.0-52-generic\n", false).unwrap();
        struct Parsed(String);
        impl OsVersionSource for Parsed {
            fn version(&self) -> Option<&str> {
                Some(&self.0)
            }
        }
        let source = Parsed(parsed);
        let evaluator = PlatformRuleEvaluator::new("linux", "x86_64", &source);
        assert!(evaluator.allowed(
            &item(serde_json::json!({"version":"-generic$"})),
            &HashMap::new()
        ));
        assert!(!evaluator.allowed(
            &item(serde_json::json!({"version":"^6\\.8\\.0$"})),
            &HashMap::new()
        ));
        assert_eq!(
            parse_host_version("Microsoft Windows [Version 10.0.22631.1]\r\n", true).as_deref(),
            Some("10.0.22631.1")
        );
        assert_eq!(parse_host_version("15.7\n", false).as_deref(), Some("15.7"));
        assert!(parse_host_version(" \n", false).is_none());
    }
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
