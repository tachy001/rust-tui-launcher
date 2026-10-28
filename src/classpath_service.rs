use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// Keeps metadata order while removing repeated library paths.
#[derive(Default)]
pub(crate) struct LaunchClasspath {
    libraries: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
}
impl LaunchClasspath {
    pub(crate) fn add_library(&mut self, path: PathBuf) {
        if self.seen.insert(path.clone()) {
            self.libraries.push(path);
        }
    }
    pub(crate) fn libraries(&self) -> &[PathBuf] {
        &self.libraries
    }
    pub(crate) fn render(&self, primary: &Path, separator: &str) -> Result<String> {
        self.libraries
            .iter()
            .map(PathBuf::as_path)
            .filter(|path| *path != primary)
            .chain(std::iter::once(primary))
            .map(|path| {
                let text = path
                    .to_str()
                    .context("Java classpath 路径无法表示为 Unicode")?;
                if text.contains(separator) {
                    bail!(
                        "Java classpath 路径包含分隔符 {separator}，请更换游戏目录：{}",
                        path.display()
                    );
                }
                Ok(text)
            })
            .collect::<Result<Vec<_>>>()
            .map(|entries| entries.join(separator))
    }
}

#[derive(Clone, Copy)]
enum IgnoreMode {
    FullPath,
    FileName,
}

/// Applies loader compatibility to JVM arguments after literal template expansion.
pub(crate) struct BootstrapCompatibility {
    mode: Option<IgnoreMode>,
}
impl BootstrapCompatibility {
    pub(crate) fn from_metadata(meta: &Value) -> Result<Self> {
        let mut result = Self { mode: None };
        if meta["mainClass"] != "cpw.mods.bootstraplauncher.BootstrapLauncher" {
            return Ok(result);
        }
        let libraries = meta["libraries"]
            .as_array()
            .context("BootstrapLauncher 缺少 libraries")?;
        let active = libraries
            .iter()
            .filter(|library| crate::metadata::allowed(library));
        let mut bootstrap = None;
        let mut has_loader = false;
        for library in active {
            let Some(name) = library["name"].as_str() else {
                continue;
            };
            let parts: Vec<_> = name.split(':').collect();
            if parts.len() < 3 {
                continue;
            }
            has_loader |= matches!(
                parts[0],
                "net.minecraftforge" | "net.neoforged" | "net.neoforged.fancymodloader"
            );
            if parts[0] == "cpw.mods" && parts[1] == "bootstraplauncher" && parts.len() == 3 {
                if bootstrap.is_some_and(|version| version != parts[2]) {
                    bail!("当前平台同时包含多个 BootstrapLauncher 版本，请修复实例依赖");
                }
                bootstrap = Some(parts[2]);
            }
        }
        if has_loader {
            if let Some(Ok(version)) = bootstrap.map(|version| {
                version
                    .split('.')
                    .map(str::parse::<u64>)
                    .collect::<std::result::Result<Vec<_>, _>>()
            }) {
                result.mode = Some(if version.as_slice() >= [0, 1, 17].as_slice() {
                    IgnoreMode::FileName
                } else {
                    IgnoreMode::FullPath
                });
            }
        }
        Ok(result)
    }
    pub(crate) fn rewrite(
        &self,
        jvm: &mut [String],
        libraries: &[PathBuf],
        primary: &Path,
    ) -> Result<()> {
        let Some(mode) = self.mode else {
            return Ok(());
        };
        let mut offset = 0;
        let slots: Vec<_> = crate::jvm_argument_service::argument_groups(jvm)
            .filter_map(|group| {
                let index = offset;
                offset += group.len();
                group[0].starts_with("-DignoreList=").then_some(index)
            })
            .collect();
        for index in slots {
            let argument = &mut jvm[index];
            let Some(value) = argument.strip_prefix("-DignoreList=") else {
                continue;
            };
            let original: Vec<_> = value.split(',').filter(|token| !token.is_empty()).collect();
            let mut entries: Vec<String> = match mode {
                IgnoreMode::FileName => original.iter().map(|value| (*value).into()).collect(),
                IgnoreMode::FullPath => vec![],
            };
            let primary_value = match mode {
                IgnoreMode::FullPath => primary.to_str(),
                IgnoreMode::FileName => primary.file_name().and_then(|name| name.to_str()),
            }
            .context("客户端 JAR 路径无法用于 BootstrapLauncher")?;
            if primary_value.contains(',') {
                bail!("BootstrapLauncher 忽略列表不能表示含逗号的客户端 JAR 路径");
            }
            if !entries.iter().any(|entry| entry == primary_value) {
                entries.push(primary_value.into());
            }
            if matches!(mode, IgnoreMode::FullPath) {
                for library in libraries {
                    let path = library
                        .to_str()
                        .context("库路径无法用于 BootstrapLauncher")?;
                    let name = library
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("库文件名无效")?;
                    if original
                        .iter()
                        .any(|token| name.contains(token) || path == *token)
                    {
                        if path.contains(',') {
                            bail!("旧 BootstrapLauncher 忽略列表不能表示含逗号的库路径：{path}");
                        }
                        if !entries.iter().any(|entry| entry == path) {
                            entries.push(path.into());
                        }
                    }
                }
            }
            *argument = format!("-DignoreList={}", entries.join(","));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata(version: &str) -> Value {
        serde_json::json!({"mainClass":"cpw.mods.bootstraplauncher.BootstrapLauncher", "libraries":[
            {"name":format!("cpw.mods:bootstraplauncher:{version}")}, {"name":"net.neoforged:coremods:7.0.3"}
        ]})
    }
    #[test]
    fn classpath_preserves_order_removes_duplicates_and_keeps_primary_last() {
        let mut cp = LaunchClasspath::default();
        for path in ["a.jar", "b.jar", "a.jar", "client.jar"] {
            cp.add_library(path.into());
        }
        assert_eq!(
            cp.render(Path::new("client.jar"), ";").unwrap(),
            "a.jar;b.jar;client.jar"
        );
        assert_eq!(
            cp.render(Path::new("client.jar"), ":").unwrap(),
            "a.jar:b.jar:client.jar"
        );
        cp.add_library("a directory/中文.jar".into());
        assert!(cp
            .render(Path::new("client.jar"), ";")
            .unwrap()
            .contains("a directory/中文.jar"));
    }
    #[test]
    fn delimiter_in_a_path_is_not_silently_split_into_multiple_jars() {
        let mut cp = LaunchClasspath::default();
        cp.add_library("wrong;directory/library.jar".into());
        assert!(cp.render(Path::new("client.jar"), ";").is_err());
        assert!(LaunchClasspath::default()
            .render(Path::new("wrong:client.jar"), ":")
            .is_err());
    }
    #[test]
    fn modern_bootstrap_ignores_the_actual_inherited_client_name_idempotently() {
        for version in ["0.1.17", "1.1.2", "2.0.2"] {
            let compatibility = BootstrapCompatibility::from_metadata(&metadata(version)).unwrap();
            let mut jvm = vec!["-DignoreList=client-extra,renamed-instance.jar".into()];
            compatibility
                .rewrite(&mut jvm, &[], Path::new("versions/1.21.1/1.21.1.jar"))
                .unwrap();
            assert_eq!(
                jvm,
                ["-DignoreList=client-extra,renamed-instance.jar,1.21.1.jar"]
            );
            compatibility
                .rewrite(&mut jvm, &[], Path::new("versions/1.21.1/1.21.1.jar"))
                .unwrap();
            assert_eq!(jvm[0].matches("1.21.1.jar").count(), 1);
        }
    }
    #[test]
    fn old_bootstrap_matches_file_names_instead_of_parent_directories() {
        let compatibility = BootstrapCompatibility::from_metadata(&metadata("0.1.16")).unwrap();
        let libraries = vec![
            "asm/libraries/asm-9.jar".into(),
            "asm/libraries/unrelated.jar".into(),
        ];
        let mut jvm = vec![
            "-DignoreList=asm,,client-extra".into(),
            "-Dother=asm".into(),
        ];
        compatibility
            .rewrite(
                &mut jvm,
                &libraries,
                Path::new("asm/versions/base/base.jar"),
            )
            .unwrap();
        assert_eq!(
            jvm[0],
            "-DignoreList=asm/versions/base/base.jar,asm/libraries/asm-9.jar"
        );
        assert_eq!(jvm[1], "-Dother=asm");
    }
    #[test]
    fn unknown_versions_or_other_launchers_are_not_rewritten() {
        let mut metas = vec![
            metadata("0.1.17-beta"),
            metadata("2.0.2"),
            metadata("2.0.2"),
        ];
        metas[1]["mainClass"] = "net.minecraft.client.main.Main".into();
        metas[2]["libraries"].as_array_mut().unwrap().pop();
        for meta in metas {
            let mut jvm = vec!["-DignoreList=original".into()];
            BootstrapCompatibility::from_metadata(&meta)
                .unwrap()
                .rewrite(&mut jvm, &[], Path::new("base.jar"))
                .unwrap();
            assert_eq!(jvm, ["-DignoreList=original"]);
        }
    }
    #[test]
    fn bootstrap_rewrite_preserves_option_operands() {
        for version in ["0.1.16", "2.0.2"] {
            let compatibility = BootstrapCompatibility::from_metadata(&metadata(version)).unwrap();
            let mut jvm = vec![
                "--module-path".into(),
                "-DignoreList=literal-directory".into(),
                "-DignoreList=asm".into(),
            ];
            compatibility
                .rewrite(
                    &mut jvm,
                    &["libraries/asm.jar".into()],
                    Path::new("versions/base/base.jar"),
                )
                .unwrap();
            assert_eq!(jvm[1], "-DignoreList=literal-directory");
            assert!(jvm[2].contains("base.jar"));
            let mut operand_only = vec!["-cp".into(), "-DignoreList=literal-directory".into()];
            compatibility
                .rewrite(&mut operand_only, &[], Path::new("invalid,client.jar"))
                .unwrap();
            assert_eq!(operand_only[1], "-DignoreList=literal-directory");
        }
    }
    #[test]
    fn incompatible_versions_and_ambiguous_comma_paths_are_explicit_errors() {
        let mut meta = metadata("2.0.2");
        meta["libraries"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name":"cpw.mods:bootstraplauncher:0.1.16"}));
        assert!(BootstrapCompatibility::from_metadata(&meta).is_err());
        let compatibility = BootstrapCompatibility::from_metadata(&metadata("0.1.16")).unwrap();
        assert!(compatibility
            .rewrite(
                &mut ["-DignoreList=asm".into()],
                &[],
                Path::new("directory,comma/base.jar")
            )
            .is_err());
    }
    #[test]
    #[ignore = "requires a cached Forge/NeoForge instance; reads metadata without launching Java"]
    fn cached_inherited_bootstrap_profile_ignores_the_actual_primary_jar() {
        let instance = PathBuf::from(
            std::env::var_os("TUI_LAUNCHER_CACHED_BOOTSTRAP_INSTANCE")
                .expect("set cached instance path"),
        );
        let id = instance.file_name().unwrap().to_str().unwrap();
        let root = instance.parent().unwrap().parent().unwrap();
        let resolved = crate::version::resolve(root, id).unwrap();
        assert_ne!(
            id, resolved.jar_id,
            "fixture must inherit a differently named client JAR"
        );
        let primary = root
            .join("versions")
            .join(&resolved.jar_id)
            .join(format!("{}.jar", resolved.jar_id));
        let values = std::collections::HashMap::from([
            ("version_name", id.to_owned()),
            ("primary_jar", primary.display().to_string()),
            ("primary_jar_name", format!("{}.jar", resolved.jar_id)),
        ]);
        let mut jvm = crate::metadata::arguments(&resolved.meta["arguments"]["jvm"])
            .into_iter()
            .filter(|argument| argument.starts_with("-DignoreList="))
            .map(|argument| crate::launch_options::expand_argument(&argument, &values).unwrap())
            .collect::<Vec<_>>();
        assert!(!jvm.is_empty());
        BootstrapCompatibility::from_metadata(&resolved.meta)
            .unwrap()
            .rewrite(&mut jvm, &[], &primary)
            .unwrap();
        assert!(jvm.iter().all(|argument| argument
            .strip_prefix("-DignoreList=")
            .unwrap()
            .split(',')
            .any(|entry| entry == format!("{}.jar", resolved.jar_id))));
    }
}
