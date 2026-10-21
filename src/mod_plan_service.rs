use crate::{
    backend::Backend,
    download::{valid_file, Download},
    metadata::{safe_path, valid_id},
    mod_dependency_service::ModPlanValidator,
    modrinth::{primary_file, ModVersion},
};
use anyhow::Result;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub(crate) trait ModArtifactSource {
    fn artifact(&self, version: &ModVersion) -> Result<PathBuf>;
}

/// Candidate files stay separate from installed Mods and are reusable across searches.
pub(crate) struct StagedModArtifacts<'a> {
    backend: &'a Backend,
    game_dir: &'a Path,
    cache: PathBuf,
}
impl<'a> StagedModArtifacts<'a> {
    pub(crate) fn new(backend: &'a Backend, game_dir: &'a Path) -> Self {
        Self {
            backend,
            game_dir,
            cache: game_dir.join("tui-mc-mod-downloads/candidates"),
        }
    }
}
impl ModArtifactSource for StagedModArtifacts<'_> {
    fn artifact(&self, version: &ModVersion) -> Result<PathBuf> {
        self.backend.control.check()?;
        valid_id(&version.id)?;
        let file = primary_file(version)?;
        let hash = &file.hashes["sha1"];
        for name in [file.filename.clone(), format!("{}.disabled", file.filename)] {
            let path = safe_path(&self.game_dir.join("mods"), &name)?;
            if valid_file(&path, Some(hash), Some(file.size))? {
                return Ok(path);
            }
        }
        let destination = safe_path(&self.cache.join(&version.id), &file.filename)?;
        self.backend.downloader().fetch(&Download {
            url: file.url.clone(),
            dest: destination.clone(),
            sha1: Some(hash.clone()),
            size: Some(file.size),
            alternatives: vec![],
        })?;
        Ok(destination)
    }
}

pub(crate) struct InstalledArtifact {
    pub path: PathBuf,
    pub project: Option<String>,
}
pub(crate) struct FabricPlanValidator<'a> {
    artifacts: &'a dyn ModArtifactSource,
    installed: Vec<InstalledArtifact>,
    builtins: HashMap<String, String>,
    activation: Option<&'a crate::mod_install_service::ModInstallState>,
}
impl<'a> FabricPlanValidator<'a> {
    pub(crate) fn new(
        artifacts: &'a dyn ModArtifactSource,
        installed: Vec<InstalledArtifact>,
        builtins: HashMap<String, String>,
    ) -> Self {
        Self {
            artifacts,
            installed,
            builtins,
            activation: None,
        }
    }
    pub(crate) fn with_activation(
        mut self,
        activation: &'a crate::mod_install_service::ModInstallState,
    ) -> Self {
        self.activation = Some(activation);
        self
    }
}
impl ModPlanValidator for FabricPlanValidator<'_> {
    fn validate(&self, plan: &[ModVersion]) -> Result<Option<String>> {
        let mut paths = self
            .installed
            .iter()
            .filter(|installed| {
                !installed.project.as_ref().is_some_and(|project| {
                    plan.iter().any(|version| &version.project_id == project)
                })
            })
            .map(|installed| installed.path.clone())
            .collect::<Vec<_>>();
        for version in plan {
            if let Some(activation) = self.activation {
                if !activation
                    .state(&version.project_id, &primary_file(version)?.filename)?
                    .enabled()
                {
                    continue;
                }
            }
            paths.push(self.artifacts.artifact(version)?);
        }
        paths.sort();
        paths.dedup();
        let errors = crate::fabric_check::conflicts(&paths, &self.builtins)?;
        Ok(if errors.is_empty() {
            None
        } else {
            Some(format!(
                "Fabric 包内依赖检查未通过：\n{}",
                errors.join("\n")
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mod_dependency_service::{ModDependencyResolver, ModRequest},
        modrinth::{Dependency, ModFile, Target},
    };
    use serde_json::json;
    use sha1::{Digest, Sha1};
    use std::{fs, io::Write};
    struct Files {
        paths: HashMap<String, PathBuf>,
    }
    impl ModArtifactSource for Files {
        fn artifact(&self, version: &ModVersion) -> Result<PathBuf> {
            Ok(self.paths[&version.id].clone())
        }
    }
    fn fixture(
        root: &Path,
        id: &str,
        project: &str,
        manifest: serde_json::Value,
    ) -> (ModVersion, PathBuf) {
        let path = root.join(format!("{id}.jar"));
        let mut archive = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        archive
            .start_file("fabric.mod.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(manifest.to_string().as_bytes()).unwrap();
        archive.finish().unwrap();
        let bytes = fs::read(&path).unwrap();
        (
            ModVersion {
                id: id.into(),
                project_id: project.into(),
                name: project.into(),
                version_number: id.into(),
                version_type: "release".into(),
                game_versions: vec!["1.21.1".into()],
                loaders: vec!["fabric".into()],
                files: vec![ModFile {
                    url: "https://must-not-be-requested.invalid/mod.jar".into(),
                    filename: format!("{project}.jar"),
                    hashes: HashMap::from([("sha1".into(), format!("{:x}", Sha1::digest(&bytes)))]),
                    primary: true,
                    size: bytes.len() as u64,
                }],
                dependencies: vec![],
                date_published: id.into(),
            },
            path,
        )
    }
    #[test]
    fn fabric_jar_range_backtracks_from_new_sodium_to_the_compatible_release() {
        let workspace = crate::test_support::Workspace::new();
        let (mut iris, iris_path) = fixture(
            &workspace.path,
            "iris1",
            "iris",
            json!({"id":"iris","version":"1.8.8","depends":{"sodium":"0.6.x"}}),
        );
        iris.dependencies.push(Dependency {
            project_id: Some("sodium".into()),
            version_id: None,
            dependency_type: "required".into(),
        });
        let (new, new_path) = fixture(
            &workspace.path,
            "sodium2",
            "sodium",
            json!({"id":"sodium","version":"0.8.13"}),
        );
        let (old, old_path) = fixture(
            &workspace.path,
            "sodium1",
            "sodium",
            json!({"id":"sodium","version":"0.6.13"}),
        );
        let files = Files {
            paths: HashMap::from([
                ("iris1".into(), iris_path),
                ("sodium2".into(), new_path),
                ("sodium1".into(), old_path),
            ]),
        };
        let versions = vec![iris, new, old];
        let catalog = |request: &ModRequest| {
            Ok(versions
                .iter()
                .filter(|version| {
                    request.version.as_ref().map_or_else(
                        || request.project.as_ref() == Some(&version.project_id),
                        |id| id == &version.id,
                    )
                })
                .cloned()
                .collect())
        };
        let target = Target {
            minecraft: "1.21.1".into(),
            loader: "fabric".into(),
        };
        let validator = FabricPlanValidator::new(&files, vec![], HashMap::new());
        let request = ModRequest {
            project: Some("iris".into()),
            version: Some("iris1".into()),
            expected_project: Some("iris".into()),
        };
        let plan = ModDependencyResolver::new(&target, &catalog)
            .with_validator(&validator)
            .resolve(request.clone())
            .unwrap();
        assert_eq!(
            plan.iter()
                .find(|version| version.project_id == "sodium")
                .unwrap()
                .id,
            "sodium1"
        );
        let mut pinned = versions.clone();
        pinned[0].dependencies[0].version_id = Some("sodium2".into());
        let fixed = |request: &ModRequest| {
            Ok(pinned
                .iter()
                .filter(|version| {
                    request.version.as_ref().map_or_else(
                        || request.project.as_ref() == Some(&version.project_id),
                        |id| id == &version.id,
                    )
                })
                .cloned()
                .collect())
        };
        assert!(ModDependencyResolver::new(&target, &fixed)
            .with_validator(&validator)
            .resolve(request)
            .err()
            .unwrap()
            .to_string()
            .contains("0.6.x"));
    }
    #[test]
    fn disabled_candidates_are_not_counted_as_enabled_dependencies() {
        struct MustNotDownload;
        impl ModArtifactSource for MustNotDownload {
            fn artifact(&self, _version: &ModVersion) -> Result<PathBuf> {
                anyhow::bail!("禁用候选不应被作为启用组合读取");
            }
        }
        let workspace = crate::test_support::Workspace::new();
        fs::create_dir(workspace.path.join("mods")).unwrap();
        fs::write(workspace.path.join("mods/old.jar.disabled"), b"old").unwrap();
        let (candidate, _) = fixture(
            &workspace.path,
            "root2",
            "root",
            json!({"id":"root","version":"2","depends":{"missing":"*"}}),
        );
        let current = crate::modrinth::Inventory {
            entries: std::collections::BTreeMap::from([(
                "root".into(),
                crate::modrinth::InstalledMod {
                    project: "root".into(),
                    version: "root1".into(),
                    version_number: "1".into(),
                    filename: "old.jar".into(),
                    sha1: "0".repeat(40),
                    size: 3,
                    published: String::new(),
                },
            )]),
        };
        let activation =
            crate::mod_install_service::ModInstallState::new(&workspace.path, &current);
        let source = MustNotDownload;
        let validator =
            FabricPlanValidator::new(&source, vec![], HashMap::new()).with_activation(&activation);
        assert!(validator
            .validate(std::slice::from_ref(&candidate))
            .unwrap()
            .is_none());
        let (_, enabled) = fixture(
            &workspace.path,
            "enabled1",
            "enabled",
            json!({"id":"enabled","version":"1","depends":{"root":">=1"}}),
        );
        let validator = FabricPlanValidator::new(
            &source,
            vec![InstalledArtifact {
                path: enabled,
                project: None,
            }],
            HashMap::new(),
        )
        .with_activation(&activation);
        assert!(validator
            .validate(&[candidate])
            .unwrap()
            .unwrap()
            .contains("未安装"));
    }
    #[test]
    fn verified_disabled_mod_is_reused_without_network_or_changing_its_state() {
        let workspace = crate::test_support::Workspace::new();
        let (version, path) = fixture(
            &workspace.path,
            "sodium1",
            "sodium",
            json!({"id":"sodium","version":"0.6.13"}),
        );
        fs::create_dir(workspace.path.join("mods")).unwrap();
        let disabled = workspace.path.join("mods/sodium.jar.disabled");
        fs::copy(&path, &disabled).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let backend = Backend::new(tx).unwrap();
        let source = StagedModArtifacts::new(&backend, &workspace.path);
        assert_eq!(source.artifact(&version).unwrap(), disabled);
        assert!(!workspace.path.join("mods/sodium.jar").exists());
        assert!(!workspace.path.join("tui-mc-mod-downloads").exists());
    }
    #[test]
    fn artifact_io_failure_aborts_search_without_trying_other_versions() {
        struct Failing(std::cell::Cell<usize>);
        impl ModArtifactSource for Failing {
            fn artifact(&self, _version: &ModVersion) -> Result<PathBuf> {
                self.0.set(self.0.get() + 1);
                anyhow::bail!("候选下载失败");
            }
        }
        let workspace = crate::test_support::Workspace::new();
        let (new, _) = fixture(
            &workspace.path,
            "root2",
            "root",
            json!({"id":"root","version":"2"}),
        );
        let (old, _) = fixture(
            &workspace.path,
            "root1",
            "root",
            json!({"id":"root","version":"1"}),
        );
        let catalog = |_request: &ModRequest| Ok(vec![new.clone(), old.clone()]);
        let source = Failing(std::cell::Cell::new(0));
        let validator = FabricPlanValidator::new(&source, vec![], HashMap::new());
        let target = Target {
            minecraft: "1.21.1".into(),
            loader: "fabric".into(),
        };
        let request = ModRequest {
            project: Some("root".into()),
            version: None,
            expected_project: Some("root".into()),
        };
        assert_eq!(
            ModDependencyResolver::new(&target, &catalog)
                .with_validator(&validator)
                .resolve(request)
                .err()
                .unwrap()
                .to_string(),
            "候选下载失败"
        );
        assert_eq!(source.0.get(), 1);
    }
    #[test]
    #[ignore = "requires existing Iris / Sodium cache; reads local JARs only"]
    fn existing_iris_sodium_jars_choose_the_compatible_dependency_without_a_game_launch() {
        let root = PathBuf::from(
            std::env::var_os("TUI_LAUNCHER_CACHED_FABRIC_INSTANCE")
                .expect("set the existing Fabric instance path"),
        );
        let iris_path = root.join("mods/iris-fabric-1.8.8+mc1.21.1.jar");
        let old_path = root.join("mods/sodium-fabric-0.6.13+mc1.21.1.jar");
        let new_path = fs::read_dir(root.join("tui-mc-mod-backups"))
            .unwrap()
            .find_map(|entry| {
                let path = entry.ok()?.path().join("sodium-fabric-0.8.13+mc1.21.1.jar");
                path.is_file().then_some(path)
            })
            .expect("missing cached Sodium 0.8.13");
        let make = |id: &str, project: &str, path: &Path| {
            let bytes = fs::read(path).unwrap();
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
            let manifest: serde_json::Value =
                serde_json::from_reader(archive.by_name("fabric.mod.json").unwrap()).unwrap();
            ModVersion {
                id: id.into(),
                project_id: project.into(),
                name: project.into(),
                version_number: manifest["version"].as_str().unwrap().into(),
                version_type: "release".into(),
                game_versions: vec!["1.21.1".into()],
                loaders: vec!["fabric".into()],
                dependencies: vec![],
                date_published: id.into(),
                files: vec![ModFile {
                    url: "https://must-not-be-requested.invalid/cached.jar".into(),
                    filename: path.file_name().unwrap().to_string_lossy().into_owned(),
                    hashes: HashMap::from([("sha1".into(), format!("{:x}", Sha1::digest(&bytes)))]),
                    primary: true,
                    size: bytes.len() as u64,
                }],
            }
        };
        let mut iris = make("iris1", "iris", &iris_path);
        iris.dependencies.push(Dependency {
            project_id: Some("sodium".into()),
            version_id: None,
            dependency_type: "required".into(),
        });
        let versions = [
            iris,
            make("sodium2", "sodium", &new_path),
            make("sodium1", "sodium", &old_path),
        ];
        let files = Files {
            paths: HashMap::from([
                ("iris1".into(), iris_path),
                ("sodium2".into(), new_path),
                ("sodium1".into(), old_path),
            ]),
        };
        let catalog = |request: &ModRequest| {
            Ok(versions
                .iter()
                .filter(|version| {
                    request.version.as_ref().map_or_else(
                        || request.project.as_ref() == Some(&version.project_id),
                        |id| id == &version.id,
                    )
                })
                .cloned()
                .collect())
        };
        let game_root = root.parent().unwrap().parent().unwrap();
        let profile =
            crate::version::resolve(game_root, root.file_name().unwrap().to_str().unwrap())
                .unwrap();
        let validator = FabricPlanValidator::new(
            &files,
            vec![],
            crate::fabric_check::builtins(&profile.meta, "1.21.1", Some(21)),
        );
        let target = Target {
            minecraft: "1.21.1".into(),
            loader: "fabric".into(),
        };
        let plan = ModDependencyResolver::new(&target, &catalog)
            .with_validator(&validator)
            .resolve(ModRequest {
                project: Some("iris".into()),
                version: Some("iris1".into()),
                expected_project: Some("iris".into()),
            })
            .unwrap();
        let selected = plan
            .iter()
            .find(|version| version.project_id == "sodium")
            .unwrap();
        assert_eq!(selected.id, "sodium1");
        println!(
            "Cached Iris 1.8.8: selected Sodium {}",
            selected.version_number
        );
    }
    #[test]
    fn corrupt_artifact_aborts_without_treating_io_failure_as_a_version_conflict() {
        let workspace = crate::test_support::Workspace::new();
        let (version, path) = fixture(
            &workspace.path,
            "broken1",
            "broken",
            json!({"id":"broken","version":"1"}),
        );
        fs::write(&path, b"broken jar").unwrap();
        let files = Files {
            paths: HashMap::from([("broken1".into(), path)]),
        };
        let validator = FabricPlanValidator::new(&files, vec![], HashMap::new());
        assert!(validator.validate(&[version]).is_err());
    }
}
