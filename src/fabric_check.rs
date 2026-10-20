use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::{
    cmp::Ordering,
    collections::HashMap,
    fs,
    io::{Cursor, Read, Seek},
    path::PathBuf,
};

#[derive(Debug)]
struct Version {
    numbers: Vec<u64>,
    pre: Option<Vec<String>>,
}
fn version(text: &str) -> Option<Version> {
    let text = text.split('+').next()?;
    let (base, pre) = text.split_once('-').map_or((text, None), |(base, pre)| {
        (base, Some(pre.split('.').map(str::to_owned).collect()))
    });
    let numbers: Option<Vec<u64>> = base.split('.').map(|part| part.parse().ok()).collect();
    Some(Version {
        numbers: numbers?,
        pre,
    })
}
fn compare(a: &Version, b: &Version) -> Ordering {
    for index in 0..a.numbers.len().max(b.numbers.len()) {
        let order = a
            .numbers
            .get(index)
            .unwrap_or(&0)
            .cmp(b.numbers.get(index).unwrap_or(&0));
        if order != Ordering::Equal {
            return order;
        }
    }
    match (&a.pre, &b.pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            for (left, right) in a.iter().zip(b) {
                if left.is_empty() != right.is_empty() {
                    return if left.is_empty() {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
                let order = match (left.parse::<u64>(), right.parse::<u64>()) {
                    (Ok(a), Ok(b)) => a.cmp(&b),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    _ => left.cmp(right),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
            a.len().cmp(&b.len())
        }
    }
}
fn predicate(actual: &str, token: &str) -> Result<bool> {
    if token == "*" {
        return Ok(true);
    }
    let (op, bound) = [">=", "<=", ">", "<", "=", "^", "~"]
        .iter()
        .find_map(|op| token.strip_prefix(op).map(|bound| (*op, bound)))
        .unwrap_or(("=", token));
    if bound.is_empty() {
        bail!("无效 Fabric 版本条件：{token}");
    }
    let parts: Vec<_> = bound.split('.').collect();
    if let Some(wildcard) = parts
        .iter()
        .position(|part| matches!(*part, "x" | "X" | "*"))
    {
        if op != "="
            || !parts[wildcard..]
                .iter()
                .all(|part| matches!(*part, "x" | "X" | "*"))
        {
            bail!("无法解析 Fabric 通配版本条件：{token}");
        }
        let Some(actual) = version(actual) else {
            return Ok(false);
        };
        for (index, part) in parts[..wildcard].iter().enumerate() {
            if actual.numbers.get(index).copied().unwrap_or(0) != part.parse::<u64>()? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    let (Some(actual_version), Some(bound_version)) = (version(actual), version(bound)) else {
        return Ok(op == "=" && actual == bound);
    };
    let order = compare(&actual_version, &bound_version);
    Ok(match op {
        "=" => order == Ordering::Equal,
        ">=" => order != Ordering::Less,
        "<=" => order != Ordering::Greater,
        ">" => order == Ordering::Greater,
        "<" => order == Ordering::Less,
        "^" | "~" => {
            let index = usize::from(op == "~");
            let mut upper = bound_version.numbers.clone();
            upper.resize(upper.len().max(index + 1), 0);
            upper[index] = upper[index].checked_add(1).context("版本范围溢出")?;
            upper.truncate(index + 1);
            let upper = Version {
                numbers: upper,
                pre: Some(vec![String::new()]),
            };
            order != Ordering::Less && compare(&actual_version, &upper) == Ordering::Less
        }
        _ => false,
    })
}
pub fn matches(actual: &str, range: &Value) -> Result<bool> {
    match range {
        Value::String(text) => {
            if text.trim().is_empty() {
                bail!("Fabric 版本条件不能为空");
            }
            for token in text.split_whitespace() {
                if !predicate(actual, token)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Value::Array(ranges) => {
            for range in ranges {
                if matches(actual, range)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => bail!("Fabric 版本条件必须是字符串或数组"),
    }
}
#[derive(Clone, Deserialize)]
struct ModInfo {
    id: String,
    version: String,
    #[serde(default)]
    depends: HashMap<String, Value>,
    #[serde(default)]
    breaks: HashMap<String, Value>,
    #[serde(default)]
    provides: Vec<String>,
}
fn read<R: Read + Seek>(
    reader: R,
    depth: usize,
    budget: &mut u64,
    infos: &mut Vec<ModInfo>,
) -> Result<()> {
    if depth > 8 || infos.len() > 2048 {
        bail!("Mod 嵌套信息过多");
    }
    let mut archive = zip::ZipArchive::new(reader)?;
    let value: Value = match archive.by_name("fabric.mod.json") {
        Ok(file) => {
            if file.size() > 1024 * 1024 {
                bail!("fabric.mod.json 超过大小限制");
            }
            serde_json::from_reader(file.take(1024 * 1024 + 1))?
        }
        Err(zip::result::ZipError::FileNotFound) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if value["environment"] == "server" {
        return Ok(());
    }
    let info: ModInfo = serde_json::from_value(value.clone())?;
    if info.id.is_empty()
        || info.id.len() > 128
        || !info
            .id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        || info.version.len() > 256
        || info.version.chars().any(char::is_control)
    {
        bail!("Mod 包含无效 ID / 版本");
    }
    infos.push(info);
    for entry in value["jars"].as_array().into_iter().flatten() {
        let name = entry["file"].as_str().context("嵌套 JAR 缺少路径")?;
        let file = archive.by_name(name)?;
        if file.size() > 32 * 1024 * 1024 || *budget < file.size() {
            bail!("嵌套 JAR 超过检查大小限制");
        }
        *budget -= file.size();
        let mut bytes = vec![];
        file.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 32 * 1024 * 1024 {
            bail!("嵌套 JAR 实际大小超限");
        }
        read(Cursor::new(bytes), depth + 1, budget, infos)?;
    }
    Ok(())
}
pub fn builtins(meta: &Value, minecraft: &str, java: Option<u64>) -> HashMap<String, String> {
    let mut result = HashMap::from([("minecraft".into(), minecraft.into())]);
    if let Some(java) = java {
        result.insert("java".into(), java.to_string());
    }
    for library in meta["libraries"].as_array().into_iter().flatten() {
        if let Some(name) = library["name"]
            .as_str()
            .and_then(|name| name.strip_prefix("net.fabricmc:fabric-loader:"))
        {
            result.insert("fabricloader".into(), name.into());
        }
    }
    result
}
pub fn audit(paths: &[PathBuf], builtins: &HashMap<String, String>) -> Result<()> {
    let mut infos = vec![];
    let mut budget = 256 * 1024 * 1024;
    for path in paths {
        read(fs::File::open(path)?, 0, &mut budget, &mut infos)
            .with_context(|| format!("检查 {}", path.display()))?;
    }
    let mut versions: HashMap<String, Vec<String>> = builtins
        .iter()
        .map(|(id, version)| (id.clone(), vec![version.clone()]))
        .collect();
    for info in &infos {
        for id in std::iter::once(&info.id).chain(&info.provides) {
            let values = versions.entry(id.clone()).or_default();
            if !values.contains(&info.version) {
                values.push(info.version.clone());
            }
        }
    }
    let mut errors = vec![];
    for info in &infos {
        for (id, range) in &info.depends {
            if id == "java" && !versions.contains_key(id) {
                continue;
            }
            let mut valid = false;
            if let Some(values) = versions.get(id) {
                for value in values {
                    valid |= matches(value, range)?;
                }
            }
            if !valid {
                errors.push(format!(
                    "{} {} 需要 {id} {range}，当前：{}",
                    info.id,
                    info.version,
                    versions
                        .get(id)
                        .map(|values| values.join(" / "))
                        .unwrap_or_else(|| "未安装".into())
                ));
            }
        }
        for (id, range) in &info.breaks {
            if let Some(values) = versions.get(id) {
                let mut all_conflict = !values.is_empty();
                for value in values {
                    all_conflict &= matches(value, range)?;
                }
                if all_conflict {
                    errors.push(format!(
                        "{} {} 与 {id} {range} 不兼容",
                        info.id, info.version
                    ));
                }
            }
        }
    }
    errors.sort();
    errors.dedup();
    if !errors.is_empty() {
        bail!("Fabric 包内依赖检查未通过：\n{}", errors.join("\n"));
    }
    Ok(())
}
pub fn active_paths(config: &crate::config::Config, id: &str) -> Result<Vec<PathBuf>> {
    let dir = crate::mods::directory(config, id)?;
    Ok(crate::mods::list(config, id)?
        .into_iter()
        .filter(|entry| entry.enabled)
        .map(|entry| dir.join(entry.filename))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fabric_ranges_follow_and_or_wildcards_and_semver_superset() {
        assert!(matches("0.6.13+mc1.21.1", &serde_json::json!(["0.6.x"])).unwrap());
        assert!(!matches("0.8.13", &serde_json::json!(["0.6.x"])).unwrap());
        assert!(matches("0.9.0", &serde_json::json!("^0.6.1")).unwrap());
        assert!(!matches("1.0.0-beta", &serde_json::json!("^0.6.1")).unwrap());
        assert!(!matches("1.0.0-0", &serde_json::json!("^0.6.1")).unwrap());
        assert!(matches("1.2.3.4", &serde_json::json!(">=1.2.3 <1.3")).unwrap());
        assert!(matches("1.2.0-beta.10", &serde_json::json!(">1.2.0-beta.2")).unwrap());
        assert!(!matches("1.3", &serde_json::json!("~1.2")).unwrap());
    }
    #[test]
    fn rejects_real_iris_sodium_range_conflict() {
        let temp = crate::test_support::Workspace::new();
        let make = |name: &str, value: Value| {
            let path = temp.path.join(name);
            let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            zip.start_file("fabric.mod.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut zip, &serde_json::to_vec(&value).unwrap()).unwrap();
            zip.finish().unwrap();
            path
        };
        let iris = make(
            "iris.jar",
            serde_json::json!({"id":"iris","version":"1.8.8","depends":{"sodium":["0.6.x"]}}),
        );
        let sodium = make(
            "sodium.jar",
            serde_json::json!({"id":"sodium","version":"0.8.13"}),
        );
        assert!(audit(&[iris.clone(), sodium], &HashMap::new())
            .unwrap_err()
            .to_string()
            .contains("0.6.x"));
        let sodium = make(
            "sodium.jar",
            serde_json::json!({"id":"sodium","version":"0.6.13"}),
        );
        audit(&[iris, sodium], &HashMap::new()).unwrap();
    }
}
