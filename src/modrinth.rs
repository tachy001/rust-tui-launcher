use crate::{
    backend::Backend,
    config::Config,
    download::{valid_file, Download},
    metadata::{safe_path, valid_id},
    storage::atomic_write,
    version,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
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
fn identifier(id: &str) -> Result<()> {
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
fn compatible(version: &ModVersion, target: &Target) -> bool {
    version.game_versions.contains(&target.minecraft) && version.loaders.contains(&target.loader)
}
fn version_rank(value: &str) -> u8 {
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
fn primary_file(version: &ModVersion) -> Result<&ModFile> {
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
#[derive(Clone)]
struct Request {
    project: Option<String>,
    version: Option<String>,
    expected_project: Option<String>,
}
fn resolve_plan(
    target: &Target,
    root: Request,
    mut fetch: impl FnMut(&Request) -> Result<ModVersion>,
) -> Result<Vec<ModVersion>> {
    let mut pending = VecDeque::from([root]);
    let mut planned: BTreeMap<String, ModVersion> = BTreeMap::new();
    while let Some(request) = pending.pop_front() {
        if planned.len() > 256 || pending.len() > 2000 {
            bail!("Mod 必需依赖数量过多，拒绝继续递归");
        }
        if let Some(known) = request
            .version
            .as_ref()
            .and_then(|id| planned.values().find(|version| &version.id == id))
        {
            if request
                .expected_project
                .as_ref()
                .is_some_and(|project| project != &known.project_id)
            {
                bail!("依赖版本与项目身份不匹配");
            }
            continue;
        }
        if request.version.is_none()
            && request
                .expected_project
                .as_ref()
                .is_some_and(|project| planned.contains_key(project))
        {
            continue;
        }
        let version = fetch(&request)?;
        identifier(&version.id)?;
        identifier(&version.project_id)?;
        if request.version.as_ref().is_some_and(|id| id != &version.id)
            || request
                .expected_project
                .as_ref()
                .is_some_and(|id| id != &version.project_id)
        {
            bail!("Modrinth 依赖响应的项目或版本身份不一致");
        }
        if !compatible(&version, target) {
            bail!(
                "依赖 {} 不支持 Minecraft {} / {}",
                version.name,
                target.minecraft,
                target.loader
            );
        }
        if let Some(old) = planned.get(&version.project_id) {
            if old.id != version.id {
                bail!(
                    "项目 {} 的必需依赖存在版本冲突：{} / {}",
                    version.project_id,
                    old.version_number,
                    version.version_number
                );
            }
            continue;
        }
        primary_file(&version)?;
        for dep in &version.dependencies {
            if dep.dependency_type != "required" {
                continue;
            }
            if dep.project_id.is_none() && dep.version_id.is_none() {
                bail!("必需依赖没有可解析的项目 / 版本 ID，请手动安装");
            }
            pending.push_back(Request {
                project: dep.project_id.clone(),
                version: dep.version_id.clone(),
                expected_project: dep.project_id.clone(),
            });
        }
        planned.insert(version.project_id.clone(), version);
    }
    for version in planned.values() {
        for dep in &version.dependencies {
            if dep.dependency_type == "incompatible"
                && planned.values().any(|candidate| {
                    dep.project_id.as_ref() == Some(&candidate.project_id)
                        || dep.version_id.as_ref() == Some(&candidate.id)
                })
            {
                bail!("待安装项目包含声明不兼容的 Mod");
            }
        }
    }
    Ok(planned.into_values().collect())
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
        let dir = config.effective(id).instance_dir(id)?;
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
        let plan = resolve_plan(&target, request, |request| {
            self.control.check()?;
            if let Some(id) = &request.version {
                identifier(id)?;
                Ok(serde_json::from_value(
                    self.get_json(&format!("{API}/version/{id}"))?,
                )?)
            } else {
                let project = request.project.as_ref().context("依赖缺少项目 ID")?;
                choose(self.versions_for(&target, project)?, &target)
            }
        })?;
        let root = config.root()?;
        let version_dir = root.join("versions").join(id);
        let _lock = crate::instances::InstanceLock::acquire(&version_dir)?;
        let game_dir = config.effective(id).instance_dir(id)?;
        let current = inventory(&game_dir)?;
        for version in &plan {
            for dep in &version.dependencies {
                if dep.dependency_type == "incompatible"
                    && current.entries.values().any(|old| {
                        (dep.project_id.as_ref() == Some(&old.project)
                            || dep.version_id.as_ref() == Some(&old.version))
                            && game_dir.join("mods").join(&old.filename).is_file()
                    })
                {
                    bail!("新 Mod 与已启用的项目不兼容，请先禁用冲突文件");
                }
            }
        }
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let staging = game_dir.join("tui-mc-mod-downloads").join(&token);
        fs::create_dir_all(&staging)?;
        let mut downloads = vec![];
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
            let already_valid = valid_file(&active, Some(&hash), Some(file.size))?;
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
            if !already_valid && valid_file(&disabled, Some(&hash), Some(file.size))? {
                fs::copy(&disabled, &dest)?;
            } else if !already_valid {
                downloads.push(Download {
                    url: file.url.clone(),
                    dest,
                    sha1: Some(hash.clone()),
                    size: Some(file.size),
                    alternatives: vec![],
                });
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
        commit(
            &game_dir,
            &staging,
            &token,
            &records,
            &current,
            &self.control,
        )?;
        self.log(format!(
            "Mod 与必需依赖安装完成，共 {} 个项目",
            records.len()
        ));
        Ok(format!("{} 个项目已安装", records.len()))
    }
    pub fn repair_mod_inventory(&self, config: &Config, id: &str) -> Result<()> {
        let dir = config.effective(id).instance_dir(id)?;
        let inventory = inventory(&dir)?;
        if inventory.entries.is_empty() {
            return Ok(());
        }
        let _lock =
            crate::instances::InstanceLock::acquire(&config.root()?.join("versions").join(id))?;
        let mods = dir.join("mods");
        let mut downloads = vec![];
        for record in inventory.entries.values() {
            self.control.check()?;
            valid_id(&record.filename)?;
            let active = safe_path(&mods, &record.filename)?;
            let disabled = safe_path(&mods, &format!("{}.disabled", record.filename))?;
            let dest = if !active.exists() && disabled.is_file() {
                disabled
            } else {
                active
            };
            if dest.is_file() && valid_file(&dest, Some(&record.sha1), Some(record.size))? {
                continue;
            }
            identifier(&record.version)?;
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

fn commit(
    dir: &Path,
    staging: &Path,
    token: &str,
    records: &[(InstalledMod, String)],
    current: &Inventory,
    control: &crate::task::TaskControl,
) -> Result<()> {
    let mods = dir.join("mods");
    fs::create_dir_all(&mods)?;
    let backup = dir.join("tui-mc-mod-backups").join(token);
    fs::create_dir_all(&backup)?;
    let old_index = fs::read(dir.join(INVENTORY)).ok();
    let receipt_path = dir.join(crate::pack_state::RECEIPT);
    let old_receipt = fs::read(&receipt_path).ok();
    let mut moved: Vec<(PathBuf, PathBuf)> = vec![];
    let mut created = vec![];
    let mut next = Inventory {
        entries: current.entries.clone(),
    };
    let result = (|| -> Result<()> {
        for (record, _) in records {
            control.check()?;
            valid_id(&record.filename)?;
            let dest = safe_path(&mods, &record.filename)?;
            if let Some(old) = current.entries.get(&record.project) {
                valid_id(&old.filename)?;
                for name in [old.filename.clone(), format!("{}.disabled", old.filename)] {
                    let path = safe_path(&mods, &name)?;
                    if path.is_file() && path != dest {
                        let saved = safe_path(&backup, &name)?;
                        fs::rename(&path, &saved)?;
                        moved.push((saved, path));
                    }
                }
            }
            if dest.exists() {
                if current.entries.values().any(|old| {
                    old.project != record.project
                        && old.filename.eq_ignore_ascii_case(&record.filename)
                }) {
                    bail!("该 JAR 文件名已由另一个项目管理：{}", record.filename);
                }
                if valid_file(&dest, Some(&record.sha1), Some(record.size))? {
                    next.entries.insert(record.project.clone(), record.clone());
                    continue;
                }
                if current
                    .entries
                    .get(&record.project)
                    .is_none_or(|old| old.filename != record.filename)
                {
                    bail!(
                        "同名文件已存在且内容不同，不会覆盖手动安装的 Mod：{}",
                        record.filename
                    );
                }
                let saved = safe_path(&backup, &record.filename)?;
                fs::rename(&dest, &saved)?;
                moved.push((saved, dest.clone()));
            }
            fs::rename(safe_path(staging, &record.filename)?, &dest)?;
            created.push(dest);
            next.entries.insert(record.project.clone(), record.clone());
        }
        atomic_write(
            &backup.join("previous-inventory.json"),
            &serde_json::to_vec_pretty(current)?,
        )?;
        atomic_write(&dir.join(INVENTORY), &serde_json::to_vec_pretty(&next)?)?;
        if let Some(bytes) = &old_receipt {
            let mut receipt: crate::pack_state::Receipt = serde_json::from_slice(bytes)?;
            for (record, url) in records {
                if let Some(old) = current.entries.get(&record.project) {
                    receipt.files.retain(|file| {
                        !file
                            .path
                            .eq_ignore_ascii_case(&format!("mods/{}", old.filename))
                    });
                }
                receipt.files.retain(|file| {
                    !file
                        .path
                        .eq_ignore_ascii_case(&format!("mods/{}", record.filename))
                });
                receipt.files.push(crate::pack_state::TrackedFile {
                    path: format!("mods/{}", record.filename),
                    sha1: record.sha1.clone(),
                    size: record.size,
                    urls: vec![url.clone()],
                    member: None,
                });
            }
            atomic_write(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in created.into_iter().rev() {
            let _ = fs::remove_file(path);
        }
        for (saved, original) in moved.into_iter().rev() {
            let _ = fs::rename(saved, original);
        }
        match old_index {
            Some(bytes) => {
                let _ = atomic_write(&dir.join(INVENTORY), &bytes);
            }
            None => {
                let _ = fs::remove_file(dir.join(INVENTORY));
            }
        }
        return Err(error).with_context(|| {
            format!(
                "Mod 安装失败，已尝试恢复原文件；备份位置：{}",
                backup.display()
            )
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
