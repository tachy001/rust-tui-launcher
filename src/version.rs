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
    resolve_guarded(root, id, None, |_| Ok(()))
}
pub(crate) fn resolve_guarded(
    root: &Path,
    id: &str,
    profile: Option<&Value>,
    mut guard: impl FnMut(&str) -> Result<()>,
) -> Result<ResolvedVersion> {
    resolve_inner(
        root,
        id,
        &mut HashSet::new(),
        profile.map(|profile| (id, profile)),
        &mut guard,
    )
}
fn resolve_inner(
    root: &Path,
    id: &str,
    seen: &mut HashSet<String>,
    candidate: Option<(&str, &Value)>,
    guard: &mut impl FnMut(&str) -> Result<()>,
) -> Result<ResolvedVersion> {
    valid_id(id)?;
    if seen.len() >= 32 || !seen.insert(id.to_owned()) {
        bail!("版本继承存在循环或超过 32 层：{id}");
    }
    guard(id)?;
    let path = root.join("versions").join(id).join(format!("{id}.json"));
    let child: Value = if let Some((_, profile)) = candidate.filter(|(target, _)| *target == id) {
        profile.clone()
    } else {
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("缺少版本 {id} 的元数据，请先安装父版本"))?,
        )?
    };
    let child = crate::version_profile_service::VersionProfileDecoder.decode(child, id)?;
    let mut resolved = if let Some(parent) = child["inheritsFrom"].as_str() {
        let mut base = resolve_inner(root, parent, seen, candidate, guard)?;
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
                let overrides: HashSet<_> = value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|lib| lib["name"].as_str().map(library_key))
                    .collect();
                libraries.retain(|old| {
                    old["name"]
                        .as_str()
                        .is_none_or(|name| !overrides.contains(&library_key(name)))
                });
                libraries.extend(value.as_array().into_iter().flatten().cloned());
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
        if child
            .pointer("/arguments/game")
            .is_some_and(Value::is_array)
        {
            object.remove("minecraftArguments");
        } else if child["minecraftArguments"].is_string() {
            if let Some(arguments) = object.get_mut("arguments").and_then(Value::as_object_mut) {
                arguments.remove("game");
            }
        }
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
    fn nullable_child_metadata_keeps_parent_client_and_arguments() {
        let temp = crate::test_support::Workspace::new();
        let root = &temp.path;
        let base = root.join("versions/base");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join("base.json"), json!({"id":"base","mainClass":"example.Main","libraries":[],"arguments":{"jvm":["-cp","${classpath}"],"game":["--base"]},"downloads":{"client":{"url":"https://example.invalid/base.jar"}}}).to_string()).unwrap();
        let child = json!({"inheritsFrom":"base","mainClass":null,"libraries":null,"arguments":{"jvm":null,"game":null},"downloads":{"client":null},"jar":null});
        let result = resolve_guarded(root, "child", Some(&child), |_| Ok(())).unwrap();
        assert_eq!(result.jar_id, "base");
        assert_eq!(result.chain, ["base", "child"]);
        assert_eq!(result.meta["mainClass"], "example.Main");
        assert_eq!(result.meta["arguments"]["game"], json!(["--base"]));
        assert_eq!(
            result.meta["arguments"]["jvm"],
            json!(["-cp", "${classpath}"])
        );
        assert_eq!(
            result.meta["downloads"]["client"]["url"],
            "https://example.invalid/base.jar"
        );
        let bad = json!({"inheritsFrom":"base","arguments":"broken"});
        let error = resolve_guarded(root, "child", Some(&bad), |_| Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("arguments"));
    }
    #[test]
    fn inheritance_preserves_child_platform_variants_and_argument_format() {
        let parent = json!({
            "libraries": [{"name":"a:b:1"}],
            "minecraftArguments":"--old",
            "arguments":{"jvm":["-Dbase=true"]}
        });
        let child = json!({
            "libraries":[
                {"name":"a:b:2","rules":[{"action":"allow","os":{"name":"windows"}}]},
                {"name":"a:b:2","rules":[{"action":"allow","os":{"name":"linux"}}]}
            ],
            "arguments":{"game":["--modern"]}
        });
        let merged = merge(&parent, &child);
        assert_eq!(merged["libraries"].as_array().unwrap().len(), 2);
        assert!(merged.get("minecraftArguments").is_none());
        assert_eq!(merged["arguments"]["jvm"], json!(["-Dbase=true"]));
        let legacy = merge(&merged, &json!({"minecraftArguments":"--legacy"}));
        assert!(legacy["arguments"].get("game").is_none());
        assert_eq!(legacy["minecraftArguments"], "--legacy");
    }
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
