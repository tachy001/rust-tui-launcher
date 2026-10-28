use crate::mod_dependency_service::ModRequest as Request;
use crate::mod_plan_service::ModArtifactSource;
#[cfg(test)]
use crate::storage::atomic_write;
use crate::{
    backend::Backend,
    config::Config,
    download::{valid_file_with_control, Download},
    metadata::{safe_path, valid_id},
    version,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

const API: &str = "https://api.modrinth.com/v2";
pub const INVENTORY: &str = "tui-mc-modrinth.json";
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModHit {
    pub project_id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub downloads: u64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct SearchResult {
    pub hits: Vec<ModHit>,
    pub total_hits: u64,
    #[serde(default)]
    pub offset: u64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ModFile {
    pub url: String,
    pub filename: String,
    pub hashes: HashMap<String, String>,
    #[serde(default)]
    pub primary: bool,
    pub size: u64,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Dependency {
    pub version_id: Option<String>,
    pub project_id: Option<String>,
    pub dependency_type: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ModVersion {
    pub id: String,
    pub project_id: String,
    pub version_number: String,
    pub name: String,
    pub version_type: String,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub files: Vec<ModFile>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub date_published: String,
}
#[derive(Clone, Debug)]
pub struct Target {
    pub minecraft: String,
    pub loader: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledMod {
    pub project: String,
    pub version: String,
    pub version_number: String,
    pub filename: String,
    pub sha1: String,
    pub size: u64,
    #[serde(default)]
    pub published: String,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default)]
    pub entries: BTreeMap<String, InstalledMod>,
}
pub fn target(config: &Config, id: &str) -> Result<Target> {
    let resolved = version::resolve(&config.root()?, id)?;
    let minecraft = resolved.chain.first().context("实例版本链为空")?.clone();
    let mut loaders = HashSet::new();
    for lib in resolved.meta["libraries"].as_array().into_iter().flatten() {
        if let Some(name) = lib["name"].as_str() {
            if name.starts_with("org.quiltmc:quilt-loader:") {
                loaders.insert("quilt");
            } else if name.starts_with("net.fabricmc:fabric-loader:") {
                loaders.insert("fabric");
            } else if name.starts_with("net.neoforged:neoforge:")
                || name.starts_with("net.neoforged:forge:")
            {
                loaders.insert("neoforge");
            } else if name.starts_with("net.minecraftforge:forge:") {
                loaders.insert("forge");
            }
        }
    }
    if loaders.len() != 1 {
        bail!("在线 Mod 下载需要明确的加载器实例，请先安装 Fabric / Quilt / Forge / NeoForge");
    }
    Ok(Target {
        minecraft,
        loader: loaders.into_iter().next().unwrap().into(),
    })
}
pub(crate) fn identifier(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        bail!("请输入 Modrinth 项目 ID、slug 或版本 ID，不要输入完整网页地址");
    }
    Ok(())
}
pub(crate) fn compatible(version: &ModVersion, target: &Target) -> bool {
    version.game_versions.contains(&target.minecraft) && version.loaders.contains(&target.loader)
}
pub(crate) fn version_rank(value: &str) -> u8 {
    match value {
        "release" => 0,
        "beta" => 1,
        "alpha" => 2,
        _ => 3,
    }
}
fn choose(mut versions: Vec<ModVersion>, target: &Target) -> Result<ModVersion> {
    versions.retain(|v| compatible(v, target));
    versions.sort_by(|a, b| {
        version_rank(&a.version_type)
            .cmp(&version_rank(&b.version_type))
            .then_with(|| b.date_published.cmp(&a.date_published))
    });
    versions
        .into_iter()
        .next()
        .context("该项目没有适用于当前 Minecraft / 加载器的版本")
}
pub(crate) fn primary_file(version: &ModVersion) -> Result<&ModFile> {
    let candidates = version
        .files
        .iter()
        .filter(|file| file.filename.to_lowercase().ends_with(".jar"))
        .collect::<Vec<_>>();
    let file = candidates
        .iter()
        .find(|file| file.primary)
        .or_else(|| candidates.first())
        .copied()
        .context("该版本没有客户端 JAR 文件")?;
    valid_id(&file.filename)?;
    let url = reqwest::Url::parse(&file.url)?;
    if url.scheme() != "https" {
        bail!("Mod 文件必须使用 HTTPS");
    }
    let hash = file.hashes.get("sha1").context("Mod 文件没有 SHA-1")?;
    if hash.len() != 40 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("Mod 文件 SHA-1 无效");
    }
    Ok(file)
}
struct ModrinthCatalog<'a> {
    backend: &'a Backend,
    target: &'a Target,
}
impl crate::mod_dependency_service::ModCatalog for ModrinthCatalog<'_> {
    fn check_task(&self) -> Result<()> {
        self.backend.control.check()
    }
    fn candidates(&self, request: &Request) -> Result<Vec<ModVersion>> {
        self.check_task()?;
        if let Some(id) = &request.version {
            identifier(id)?;
            Ok(vec![serde_json::from_value(
                self.backend.get_json(&format!("{API}/version/{id}"))?,
            )?])
        } else {
            let project = request.project.as_ref().context("依赖缺少项目 ID")?;
            self.backend.versions_for(self.target, project)
        }
    }
}
#[cfg(test)]
fn resolve_plan(
    target: &Target,
    root: Request,
    fetch: impl FnMut(&Request) -> Result<ModVersion>,
) -> Result<Vec<ModVersion>> {
    let fetch = std::cell::RefCell::new(fetch);
    let source = |request: &Request| Ok(vec![(fetch.borrow_mut())(request)?]);
    crate::mod_dependency_service::ModDependencyResolver::new(target, &source).resolve(root)
}

fn inventory(dir: &Path) -> Result<Inventory> {
    let path = dir.join(INVENTORY);
    if !path.exists() {
        return Ok(Inventory::default());
    }
    serde_json::from_slice(&fs::read(path)?).context("Modrinth 安装记录损坏")
}
impl Backend {
    pub fn search_mods(
        &self,
        config: &Config,
        id: &str,
        query: &str,
        offset: u64,
    ) -> Result<SearchResult> {
        let target = target(config, id)?;
        let facets = json!([
            ["project_type:mod"],
            [format!("versions:{}", target.minecraft)],
            [format!("categories:{}", target.loader)]
        ]);
        let mut url = reqwest::Url::parse(&format!("{API}/search"))?;
        url.query_pairs_mut()
            .append_pair("query", query)
            .append_pair("facets", &facets.to_string())
            .append_pair("limit", "20")
            .append_pair("offset", &offset.to_string());
        serde_json::from_value(self.get_json(url.as_str())?).context("Modrinth 搜索响应格式错误")
    }
    pub fn mod_versions(
        &self,
        config: &Config,
        id: &str,
        project: &str,
    ) -> Result<Vec<ModVersion>> {
        let target = target(config, id)?;
        self.versions_for(&target, project)
    }
    fn versions_for(&self, target: &Target, project: &str) -> Result<Vec<ModVersion>> {
        identifier(project)?;
        let mut url = reqwest::Url::parse(&format!("{API}/project/{project}/version"))?;
        url.query_pairs_mut()
            .append_pair("game_versions", &json!([target.minecraft]).to_string())
            .append_pair("loaders", &json!([target.loader]).to_string())
            .append_pair("include_changelog", "false");
        let mut versions: Vec<ModVersion> = serde_json::from_value(self.get_json(url.as_str())?)?;
        versions.retain(|version| compatible(version, target));
        versions.sort_by(|a, b| b.date_published.cmp(&a.date_published));
        Ok(versions)
    }
    pub fn mod_updates(&self, config: &Config, id: &str) -> Result<Vec<ModHit>> {
        let target = target(config, id)?;
        let dir = config.effective(id)?.instance_dir(id)?;
        let inventory = inventory(&dir)?;
        let mut result = vec![];
        for installed in inventory.entries.values() {
            self.control.check()?;
            let latest = match self
                .versions_for(&target, &installed.project)
                .and_then(|versions| choose(versions, &target))
            {
                Ok(version) => version,
                Err(error) => {
                    self.log(format!("跳过 {} 更新检查：{error}", installed.project));
                    continue;
                }
            };
            if latest.id != installed.version
                && (installed.published.is_empty() || latest.date_published > installed.published)
            {
                result.push(ModHit {
                    project_id: installed.project.clone(),
                    slug: installed.project.clone(),
                    title: format!("{} → {}", installed.version_number, latest.version_number),
                    description: format!("{}：可用更新，安装前会备份旧文件", latest.name),
                    downloads: 0,
                });
            }
        }
        Ok(result)
    }
    pub fn install_mod(
        &self,
        config: &Config,
        id: &str,
        project: &str,
        requested: Option<&str>,
    ) -> Result<String> {
        identifier(project)?;
        let _session = crate::local_mod_service::ModMutationSession::begin(config, id)?;
        let root = config.root()?;
        let target = target(config, id)?;
        let root_project = if requested.is_some() {
            let value = self.get_json(&format!("{API}/project/{project}"))?;
            Some(value["id"].as_str().context("项目没有 ID")?.to_owned())
        } else {
            None
        };
        let request = Request {
            project: Some(project.into()),
            version: requested.map(str::to_owned),
            expected_project: root_project,
        };
        let game_dir = config.effective(id)?.instance_dir(id)?;
        let current = inventory(&game_dir)?;
        let activation = crate::mod_install_service::ModInstallState::new(&game_dir, &current);
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let staging = game_dir.join("tui-mc-mod-downloads").join(&token);
        fs::create_dir_all(&staging)?;
        let artifacts = crate::mod_plan_service::StagedModArtifacts::new(self, &game_dir);
        let validator = if target.loader == "fabric" {
            let installed = crate::fabric_check::active_paths(config, id)?
                .into_iter()
                .map(|path| {
                    let filename = path.file_name().unwrap_or_default().to_string_lossy();
                    let project = current
                        .entries
                        .values()
                        .find(|old| old.filename.eq_ignore_ascii_case(&filename))
                        .map(|old| old.project.clone());
                    crate::mod_plan_service::InstalledArtifact { path, project }
                })
                .collect();
            let resolved = version::resolve(&root, id)?;
            Some(
                crate::mod_plan_service::FabricPlanValidator::new(
                    &artifacts,
                    installed,
                    crate::fabric_check::builtins(&resolved.meta, &target.minecraft, None),
                )
                .with_activation(&activation),
            )
        } else {
            None
        };
        let catalog = ModrinthCatalog {
            backend: self,
            target: &target,
        };
        let mut resolver =
            crate::mod_dependency_service::ModDependencyResolver::new(&target, &catalog);
        if let Some(validator) = &validator {
            resolver = resolver.with_validator(validator);
        }
        let plan = resolver.resolve(request)?;
        for version in &plan {
            if !activation
                .state(&version.project_id, &primary_file(version)?.filename)?
                .enabled()
            {
                continue;
            }
            for dep in &version.dependencies {
                if dep.dependency_type == "incompatible"
                    && current.entries.values().any(|old| {
                        crate::mod_dependency_service::dependency_matches_ids(
                            dep,
                            &old.project,
                            &old.version,
                        ) && !plan
                            .iter()
                            .any(|replacement| replacement.project_id == old.project)
                            && game_dir.join("mods").join(&old.filename).is_file()
                    })
                {
                    bail!("新 Mod 与已启用的项目不兼容，请先禁用冲突文件");
                }
            }
        }
        let mut downloads = vec![];
        let mut cached_copies = vec![];
        let mut records = vec![];
        let mut filenames = HashSet::new();
        for version in plan {
            let file = primary_file(&version)?;
            let hash = file.hashes["sha1"].clone();
            if !filenames.insert(file.filename.to_lowercase()) {
                bail!("不同项目提供同名 JAR，无法同时安装");
            }
            let dest = safe_path(&staging, &file.filename)?;
            let active = safe_path(&game_dir.join("mods"), &file.filename)?;
            if current.entries.values().any(|old| {
                old.project != version.project_id
                    && old.filename.eq_ignore_ascii_case(&file.filename)
            }) {
                bail!("该 JAR 文件名已由另一个项目管理：{}", file.filename);
            }
            let already_valid =
                valid_file_with_control(&active, Some(&hash), Some(file.size), &self.control)?;
            if active.exists()
                && !already_valid
                && current
                    .entries
                    .get(&version.project_id)
                    .is_none_or(|old| !old.filename.eq_ignore_ascii_case(&file.filename))
            {
                bail!(
                    "同名文件已存在且内容不同，不会覆盖手动安装的 Mod：{}",
                    file.filename
                );
            }
            let disabled = safe_path(
                &game_dir.join("mods"),
                &format!("{}.disabled", file.filename),
            )?;
            if !already_valid && target.loader == "fabric" {
                crate::storage::atomic_copy_controlled(
                    &dest,
                    &mut fs::File::open(artifacts.artifact(&version)?)?,
                    &self.control,
                )?;
            } else if !already_valid
                && valid_file_with_control(&disabled, Some(&hash), Some(file.size), &self.control)?
            {
                crate::storage::atomic_copy_controlled(
                    &dest,
                    &mut fs::File::open(&disabled)?,
                    &self.control,
                )?;
            } else if !already_valid {
                let download = artifacts.download(&version)?;
                cached_copies.push((download.dest.clone(), dest));
                downloads.push(download);
            }
            records.push((
                InstalledMod {
                    project: version.project_id.clone(),
                    version: version.id.clone(),
                    version_number: version.version_number.clone(),
                    filename: file.filename.clone(),
                    sha1: hash,
                    size: file.size,
                    published: version.date_published.clone(),
                },
                file.url.clone(),
            ));
            self.log(format!(
                "准备 {} {} / {}",
                version.name, version.version_number, version.version_type
            ));
        }
        self.downloader()
            .many(downloads, config.download_threads, "Mod 下载与依赖")?;
        for (cached, staged) in cached_copies {
            crate::storage::atomic_copy_controlled(
                &staged,
                &mut fs::File::open(cached)?,
                &self.control,
            )?;
        }
        if target.loader == "fabric" {
            let mut paths = crate::fabric_check::active_paths(config, id)?;
            paths.retain(|path| {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                !records.iter().any(|(record, _)| {
                    filename.eq_ignore_ascii_case(&record.filename)
                        || current
                            .entries
                            .get(&record.project)
                            .is_some_and(|old| filename.eq_ignore_ascii_case(&old.filename))
                })
            });
            for (record, _) in &records {
                if !activation
                    .state(&record.project, &record.filename)?
                    .enabled()
                {
                    continue;
                }
                let staged = safe_path(&staging, &record.filename)?;
                paths.push(if staged.is_file() {
                    staged
                } else {
                    safe_path(&game_dir.join("mods"), &record.filename)?
                });
            }
            let resolved = version::resolve(&root, id)?;
            let builtins = crate::fabric_check::builtins(&resolved.meta, &target.minecraft, None);
            crate::fabric_check::audit(&paths, &builtins)
                .context("文件已下载但尚未替换，请选择满足包内依赖范围的版本")?;
        }
        crate::mod_install_service::ModInstallTransaction::new(
            &game_dir,
            &staging,
            &token,
            &records,
            &current,
            &self.control,
        )
        .commit()?;
        self.log(format!(
            "Mod 与必需依赖安装完成，共 {} 个项目",
            records.len()
        ));
        Ok(format!("{} 个项目已安装", records.len()))
    }
    pub(crate) fn repair_mod_inventory_with_session(
        &self,
        config: &Config,
        id: &str,
        mutation: &crate::instance_service::InstanceMutation,
    ) -> Result<()> {
        mutation.require(&config.root()?, id)?;
        let _game = crate::local_mod_service::ModMutationSession::lock_game_directory(config, id)?;
        let dir = config.effective(id)?.instance_dir(id)?;
        let inventory = inventory(&dir)?;
        if inventory.entries.is_empty() {
            return Ok(());
        }
        let mods = dir.join("mods");
        let activation = crate::mod_install_service::ModInstallState::new(&dir, &inventory);
        let mut targets = vec![];
        let mut filenames = HashSet::new();
        for record in inventory.entries.values() {
            self.control.check()?;
            valid_id(&record.filename)?;
            if !filenames.insert(record.filename.to_lowercase()) {
                bail!("Mod 修复清单包含重复文件名：{}", record.filename);
            }
            let dest = activation
                .state(&record.project, &record.filename)?
                .destination(&mods, &record.filename)?;
            targets.push((record, dest));
        }
        let mut pending = vec![];
        for (record, dest) in targets {
            self.control.check()?;
            if dest.is_file()
                && valid_file_with_control(
                    &dest,
                    Some(&record.sha1),
                    Some(record.size),
                    &self.control,
                )?
            {
                continue;
            }
            identifier(&record.version)?;
            pending.push((record, dest));
        }
        let mut downloads = vec![];
        for (record, dest) in pending {
            self.control.check()?;
            let version: ModVersion = serde_json::from_value(
                self.get_json(&format!("{API}/version/{}", record.version))?,
            )?;
            if version.project_id != record.project || version.id != record.version {
                bail!("Mod 修复返回的项目身份不一致");
            }
            let file = version
                .files
                .iter()
                .find(|file| {
                    file.filename == record.filename
                        && file.hashes.get("sha1") == Some(&record.sha1)
                })
                .context("平台已不再提供原安装文件，请手动选择版本")?;
            downloads.push(Download {
                url: file.url.clone(),
                dest,
                sha1: Some(record.sha1.clone()),
                size: Some(record.size),
                alternatives: vec![],
            });
        }
        self.downloader()
            .many(downloads, config.download_threads, "在线 Mod 文件修复")
    }
}

#[cfg(test)]
fn commit(
    dir: &Path,
    staging: &Path,
    token: &str,
    records: &[(InstalledMod, String)],
    current: &Inventory,
    control: &crate::task::TaskControl,
) -> Result<()> {
    crate::mod_install_service::ModInstallTransaction::new(
        dir, staging, token, records, current, control,
    )
    .commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repair_preserves_disabled_mod_and_rejects_ambiguous_copies() {
        let temp = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: temp.path.clone(),
            isolated: false,
            ..Config::default()
        };
        fs::create_dir(temp.path.join("mods")).unwrap();
        let disabled = temp.path.join("mods/a.jar.disabled");
        let active = temp.path.join("mods/a.jar");
        fs::write(&disabled, b"abc").unwrap();
        let record = InstalledMod {
            project: "a".into(),
            version: "v1".into(),
            version_number: "1".into(),
            filename: "a.jar".into(),
            sha1: "a9993e364706816aba3e25717850c26c9cd0d89d".into(),
            size: 3,
            published: String::new(),
        };
        let inventory = Inventory {
            entries: BTreeMap::from([("a".into(), record)]),
        };
        let bytes = serde_json::to_vec(&inventory).unwrap();
        fs::write(temp.path.join(INVENTORY), &bytes).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let backend = Backend::new(tx).unwrap();
        let mutation =
            crate::instance_service::InstanceMutation::begin(&config.root().unwrap(), "test")
                .unwrap();
        backend
            .repair_mod_inventory_with_session(&config, "test", &mutation)
            .unwrap();
        assert!(!active.exists());
        fs::write(&active, b"abc").unwrap();
        let error = backend
            .repair_mod_inventory_with_session(&config, "test", &mutation)
            .unwrap_err();
        assert!(format!("{error:#}").contains("同时存在启用和禁用"));
        assert_eq!(fs::read(&active).unwrap(), b"abc");
        assert_eq!(fs::read(&disabled).unwrap(), b"abc");
        assert_eq!(fs::read(temp.path.join(INVENTORY)).unwrap(), bytes);
        // A missing earlier entry must not begin remote resolution before a later conflict.
        let mut conflicting = inventory;
        let mut missing = conflicting.entries["a"].clone();
        missing.project = "0".into();
        missing.filename = "missing.jar".into();
        missing.version = "invalid/version".into();
        conflicting.entries.insert("0".into(), missing);
        fs::write(
            temp.path.join(INVENTORY),
            serde_json::to_vec(&conflicting).unwrap(),
        )
        .unwrap();
        let error = backend
            .repair_mod_inventory_with_session(&config, "test", &mutation)
            .unwrap_err();
        assert!(format!("{error:#}").contains("同时存在启用和禁用"));
    }
    #[test]
    fn reinstall_can_adopt_verified_file_without_staged_download() {
        let temp = crate::test_support::Workspace::new();
        fs::create_dir(temp.path.join("mods")).unwrap();
        fs::write(temp.path.join("mods/a.jar"), b"abc").unwrap();
        let record = InstalledMod {
            project: "a".into(),
            version: "v1".into(),
            version_number: "1".into(),
            filename: "a.jar".into(),
            sha1: "a9993e364706816aba3e25717850c26c9cd0d89d".into(),
            size: 3,
            published: String::new(),
        };
        commit(
            &temp.path,
            &temp.path.join("no-download"),
            "reuse",
            &[(record, "https://example.com/a.jar".into())],
            &Inventory::default(),
            &crate::task::TaskControl::default(),
        )
        .unwrap();
        assert_eq!(fs::read(temp.path.join("mods/a.jar")).unwrap(), b"abc");
        assert_eq!(inventory(&temp.path).unwrap().entries["a"].version, "v1");
    }
    fn version(id: &str, project: &str, kind: &str) -> ModVersion {
        ModVersion {
            id: id.into(),
            project_id: project.into(),
            version_number: id.into(),
            name: project.into(),
            version_type: kind.into(),
            game_versions: vec!["1.21.1".into()],
            loaders: vec!["fabric".into()],
            date_published: id.into(),
            files: vec![ModFile {
                url: "https://example.com/a.jar".into(),
                filename: format!("{project}.jar"),
                hashes: HashMap::from([(
                    "sha1".into(),
                    "a9993e364706816aba3e25717850c26c9cd0d89d".into(),
                )]),
                primary: true,
                size: 3,
            }],
            dependencies: vec![],
        }
    }
    fn target() -> Target {
        Target {
            minecraft: "1.21.1".into(),
            loader: "fabric".into(),
        }
    }
    #[test]
    fn release_preferred_and_wrong_loader_excluded() {
        let mut wrong = version("z", "p", "release");
        wrong.loaders = vec!["forge".into()];
        let selected = choose(
            vec![
                version("b", "p", "beta"),
                wrong,
                version("a", "p", "release"),
            ],
            &target(),
        )
        .unwrap();
        assert_eq!(selected.id, "a");
    }
    #[test]
    fn required_dependency_pins_cannot_conflict() {
        let mut root = version("root", "root", "release");
        root.dependencies = vec![
            Dependency {
                project_id: Some("dep".into()),
                version_id: Some("v1".into()),
                dependency_type: "required".into(),
            },
            Dependency {
                project_id: Some("dep".into()),
                version_id: Some("v2".into()),
                dependency_type: "required".into(),
            },
        ];
        let result = resolve_plan(
            &target(),
            Request {
                project: Some("root".into()),
                version: None,
                expected_project: None,
            },
            |request| {
                Ok(if let Some(id) = &request.version {
                    version(id, "dep", "release")
                } else {
                    root.clone()
                })
            },
        );
        assert!(result.is_err());
    }
    #[test]
    fn failed_install_restores_existing_mods_and_metadata() {
        use sha1::Digest;
        let temp = crate::test_support::Workspace::new();
        let dir = &temp.path;
        let mods = dir.join("mods");
        let staging = dir.join("staging");
        fs::create_dir(&mods).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(mods.join("old-a.jar"), b"old").unwrap();
        fs::write(mods.join("b.jar"), b"manual").unwrap();
        fs::write(staging.join("a.jar"), b"new").unwrap();
        let old = InstalledMod {
            project: "a".into(),
            version: "v1".into(),
            version_number: "1".into(),
            filename: "old-a.jar".into(),
            sha1: format!("{:x}", sha1::Sha1::digest(b"old")),
            size: 3,
            published: String::new(),
        };
        let current = Inventory {
            entries: BTreeMap::from([("a".into(), old)]),
        };
        atomic_write(&dir.join(INVENTORY), &serde_json::to_vec(&current).unwrap()).unwrap();
        let new = InstalledMod {
            project: "a".into(),
            version: "v2".into(),
            version_number: "2".into(),
            filename: "a.jar".into(),
            sha1: format!("{:x}", sha1::Sha1::digest(b"new")),
            size: 3,
            published: String::new(),
        };
        let mut other = new.clone();
        other.project = "b".into();
        other.filename = "b.jar".into();
        let control = crate::task::TaskControl::default();
        assert!(commit(
            dir,
            &staging,
            "conflict",
            &[
                (new.clone(), "https://example.com/a.jar".into()),
                (other, "https://example.com/b.jar".into())
            ],
            &current,
            &control
        )
        .is_err());
        assert_eq!(fs::read(mods.join("old-a.jar")).unwrap(), b"old");
        assert_eq!(fs::read(mods.join("b.jar")).unwrap(), b"manual");
        assert!(!mods.join("a.jar").exists());
        assert_eq!(inventory(dir).unwrap().entries["a"].version, "v1");
        fs::write(staging.join("a.jar"), b"new").unwrap();
        fs::write(dir.join(crate::pack_state::RECEIPT), b"invalid-json").unwrap();
        assert!(commit(
            dir,
            &staging,
            "metadata",
            &[(new.clone(), "https://example.com/a.jar".into())],
            &current,
            &control
        )
        .is_err());
        assert_eq!(inventory(dir).unwrap().entries["a"].version, "v1");
        assert_eq!(fs::read(mods.join("old-a.jar")).unwrap(), b"old");
        fs::remove_file(dir.join(crate::pack_state::RECEIPT)).unwrap();
        fs::write(staging.join("a.jar"), b"new").unwrap();
        commit(
            dir,
            &staging,
            "success",
            &[(new, "https://example.com/a.jar".into())],
            &current,
            &control,
        )
        .unwrap();
        assert_eq!(fs::read(mods.join("a.jar")).unwrap(), b"new");
        assert_eq!(
            fs::read(dir.join("tui-mc-mod-backups/success/old-a.jar")).unwrap(),
            b"old"
        );
        assert_eq!(inventory(dir).unwrap().entries["a"].version, "v2");
    }
}
