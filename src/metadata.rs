use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoteVersion {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub sha1: String,
}
#[derive(Deserialize, Serialize)]
pub struct Manifest {
    pub versions: Vec<RemoteVersion>,
}
pub fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let normalized = relative.replace('\\', "/");
    let path = Path::new(&normalized);
    if normalized.contains(':')
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || normalized.is_empty()
    {
        bail!("不安全的资源路径: {relative}");
    }
    for component in path.components() {
        if let Component::Normal(name) = component {
            valid_id(&name.to_string_lossy())
                .with_context(|| format!("资源路径包含不安全的文件名：{relative}"))?;
        }
    }
    Ok(root.join(path))
}
pub fn valid_id(id: &str) -> Result<()> {
    let stem = id.split('.').next().unwrap_or("").to_ascii_uppercase();
    if id.is_empty()
        || id.len() > 180
        || id
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || id.ends_with([' ', '.'])
        || [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ]
        .contains(&stem.as_str())
        || id == "."
        || id == ".."
    {
        bail!("无效版本名称");
    }
    Ok(())
}
pub fn allowed(item: &Value) -> bool {
    allowed_with_features(item, &std::collections::HashMap::new())
}
pub fn allowed_with_features(
    item: &Value,
    features: &std::collections::HashMap<&str, bool>,
) -> bool {
    let Some(rules) = item.get("rules").and_then(Value::as_array) else {
        return true;
    };
    if rules.is_empty() {
        return true;
    }
    let mut allow = false;
    for rule in rules {
        if let Some(os) = rule.get("os") {
            if os
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n != os_name())
            {
                continue;
            }
            if let Some(arch) = os.get("arch").and_then(Value::as_str) {
                let current = match std::env::consts::ARCH {
                    "x86_64" => "amd64",
                    "x86" => "x86",
                    a => a,
                };
                if arch != current {
                    continue;
                }
            }
            // Version-specific native rules require the host's actual OS version.
            if let Some(pattern) = os.get("version").and_then(Value::as_str) {
                let version = host_version();
                if !regex::Regex::new(pattern).is_ok_and(|r| r.is_match(&version)) {
                    continue;
                }
            }
        }
        if rule
            .get("features")
            .and_then(Value::as_object)
            .is_some_and(|required| {
                required.iter().any(|(name, expected)| {
                    expected.as_bool()
                        != Some(features.get(name.as_str()).copied().unwrap_or(false))
                })
            })
        {
            continue;
        }
        allow = rule["action"] == "allow";
    }
    allow
}
fn host_version() -> String {
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
    };
    output
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            regex::Regex::new(r"\d+\.\d+(?:\.\d+)*")
                .unwrap()
                .find(&text)
                .map(|m| m.as_str().to_owned())
                .unwrap_or(text)
        })
        .unwrap_or_default()
}
pub fn arguments(value: &Value) -> Vec<String> {
    arguments_with_features(value, &std::collections::HashMap::new())
}
pub fn arguments_with_features(
    value: &Value,
    features: &std::collections::HashMap<&str, bool>,
) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .flat_map(|item| {
                    if let Some(s) = item.as_str() {
                        return vec![s.to_owned()];
                    }
                    if !allowed_with_features(item, features) {
                        return vec![];
                    }
                    match &item["value"] {
                        Value::String(s) => vec![s.clone()],
                        Value::Array(a) => a
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        _ => vec![],
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rule_order_and_features() {
        assert!(!allowed(
            &json!({"rules":[{"action":"allow"},{"action":"disallow"}]})
        ));
        assert!(!allowed(
            &json!({"rules":[{"action":"allow","features":{"is_demo_user":true}}]})
        ));
        assert!(allowed(
            &json!({"rules":[{"action":"allow","os":{"name":os_name()}}]})
        ));
    }
    #[test]
    fn rejects_traversal() {
        for p in [
            "../secret",
            "C:/secret",
            "a/../../b",
            "/etc/passwd",
            "..\\secret",
        ] {
            assert!(safe_path(Path::new("root"), p).is_err());
        }
        assert!(safe_path(Path::new("root"), "org/example/a.jar").is_ok());
    }
    #[test]
    fn conditional_arguments() {
        assert_eq!(
            arguments(&json!(["--a", {"rules":[{"action":"allow"}],"value":["--b","c"]}])),
            vec!["--a", "--b", "c"]
        );
    }
}
