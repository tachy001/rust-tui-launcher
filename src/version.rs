use crate::metadata::{safe_path, valid_id};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{collections::HashSet, fs, path::Path};

#[derive(Debug)]
pub struct ResolvedVersion {
    pub meta: Value,
    pub jar_id: String,
    pub chain: Vec<String>,
}
pub fn resolve(root: &Path, id: &str) -> Result<ResolvedVersion> {
    resolve_inner(root, id, &mut HashSet::new())
}
fn resolve_inner(root: &Path, id: &str, seen: &mut HashSet<String>) -> Result<ResolvedVersion> {
    valid_id(id)?;
    if seen.len() >= 32 || !seen.insert(id.to_owned()) {
        bail!("版本继承存在循环或超过 32 层：{id}");
    }
    let path = root.join("versions").join(id).join(format!("{id}.json"));
    let child: Value = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("缺少版本 {id} 的元数据，请先安装父版本"))?,
    )?;
    if !child.is_object() {
        bail!("版本元数据必须为 JSON 对象：{id}");
    }
    let mut resolved = if let Some(parent) = child["inheritsFrom"].as_str() {
        let mut base = resolve_inner(root, parent, seen)?;
        base.meta = merge(&base.meta, &child);
        base.chain.push(id.to_owned());
        if child.pointer("/downloads/client").is_some() {
            base.jar_id = id.to_owned();
        }
        base
    } else {
        ResolvedVersion {
            meta: child.clone(),
            jar_id: id.to_owned(),
            chain: vec![id.to_owned()],
        }
    };
    if let Some(jar) = child["jar"].as_str() {
        valid_id(jar)?;
        resolved.jar_id = jar.to_owned();
    }
    resolved.meta["id"] = json!(id);
    Ok(resolved)
}
pub fn merge(parent: &Value, child: &Value) -> Value {
    let mut result = parent.clone();
    for (key, value) in child.as_object().into_iter().flatten() {
        match key.as_str() {
            "libraries" => {
                let mut libraries = parent[key].as_array().cloned().unwrap_or_default();
                for lib in value.as_array().into_iter().flatten() {
                    if let Some(name) = lib["name"].as_str() {
                        let identity = library_key(name);
                        libraries.retain(|old| {
                            old["name"]
                                .as_str()
                                .is_none_or(|n| library_key(n) != identity)
                        });
                    }
                    libraries.push(lib.clone());
                }
                result[key] = json!(libraries);
            }
            "arguments" => {
                for kind in ["jvm", "game"] {
                    let mut args = parent[key][kind].as_array().cloned().unwrap_or_default();
                    args.extend(value[kind].as_array().into_iter().flatten().cloned());
                    result[key][kind] = json!(args);
                }
            }
            "downloads" => {
                for (kind, entry) in value.as_object().into_iter().flatten() {
                    result[key][kind] = entry.clone();
                }
            }
            "inheritsFrom" => {}
            _ => result[key] = value.clone(),
        }
    }
    if let Some(object) = result.as_object_mut() {
        object.remove("inheritsFrom");
    }
    result
}
fn library_key(name: &str) -> String {
    let (coordinate, extension) = name.split_once('@').unwrap_or((name, "jar"));
    let parts: Vec<_> = coordinate.split(':').collect();
    if parts.len() >= 3 {
        format!(
            "{}:{}:{}@{extension}",
            parts[0],
            parts[1],
            parts.get(3).copied().unwrap_or("")
        )
    } else {
        name.to_owned()
    }
}
pub fn maven_path(name: &str) -> Result<String> {
    let (coordinate, extension) = name.split_once('@').unwrap_or((name, "jar"));
    let parts: Vec<_> = coordinate.split(':').collect();
    if !(3..=4).contains(&parts.len()) || parts.iter().any(|s| s.is_empty()) || extension.is_empty()
    {
        bail!("无效 Maven 坐标：{name}");
    }
    let classifier = parts.get(3).map(|c| format!("-{c}")).unwrap_or_default();
    let path = format!(
        "{}/{}/{}/{}-{}{classifier}.{extension}",
        parts[0].replace('.', "/"),
        parts[1],
        parts[2],
        parts[1],
        parts[2]
    );
    safe_path(Path::new("libraries"), &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_overrides_library_but_keeps_other_classifiers() {
        let merged = merge(
            &json!({"id":"base", "libraries":[{"name":"a:b:1"},{"name":"a:b:1:natives"}], "arguments":{"jvm":["-cp","${classpath}"],"game":["--username","${auth_player_name}"]},"downloads":{"client":{"url":"base"}}}),
            &json!({"id":"mod", "inheritsFrom":"base", "libraries":[{"name":"a:b:2"}],"arguments":{"jvm":["-Dloader=true"]}}),
        );
        assert_eq!(merged["libraries"][0]["name"], "a:b:1:natives");
        assert_eq!(merged["libraries"][1]["name"], "a:b:2");
        assert_eq!(merged["arguments"]["jvm"].as_array().unwrap().len(), 3);
        assert_eq!(merged["downloads"]["client"]["url"], "base");
        assert!(merged.get("inheritsFrom").is_none());
    }
    #[test]
    fn coordinate_paths() {
        assert_eq!(
            maven_path("net.fabricmc:fabric-loader:0.16.0").unwrap(),
            "net/fabricmc/fabric-loader/0.16.0/fabric-loader-0.16.0.jar"
        );
        assert_eq!(
            maven_path("a.b:c:1:native@zip").unwrap(),
            "a/b/c/1/c-1-native.zip"
        );
        assert!(maven_path("a:b:../../evil").is_err());
    }
    #[test]
    fn cycle_is_reported() {
        let root = std::env::temp_dir().join(format!("tui-mc-cycle-{}", std::process::id()));
        for (id, parent) in [("a", "b"), ("b", "a")] {
            let dir = root.join("versions").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("{id}.json")),
                json!({"inheritsFrom":parent}).to_string(),
            )
            .unwrap();
        }
        assert!(resolve(&root, "a")
            .unwrap_err()
            .to_string()
            .contains("循环"));
        for id in ["a", "b"] {
            let dir = root.join("versions").join(id);
            fs::remove_file(dir.join(format!("{id}.json"))).unwrap();
            fs::remove_dir(dir).unwrap();
        }
        fs::remove_dir(root.join("versions")).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
