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
    crate::platform_rule_service::PlatformRuleEvaluator::current().allowed(item, features)
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
