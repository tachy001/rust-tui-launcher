use crate::{
    backend,
    config::Config,
    metadata::{safe_path, valid_id},
    storage::atomic_write,
    task::TaskControl,
    version,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const DATA_DIRS: &[&str] = &[
    "mods",
    "config",
    "defaultconfigs",
    "resourcepacks",
    "shaderpacks",
    "scripts",
    "kubejs",
    "patchouli_books",
    "fancymenu",
    "journeymap",
    "xaero",
    "saves",
    "screenshots",
];
const DATA_FILES: &[&str] = &[
    "options.txt",
    "optionsof.txt",
    "optionsshaders.txt",
    "servers.dat",
    "servers.dat_old",
    "icon.png",
];
static SERIAL: AtomicU64 = AtomicU64::new(0);
#[derive(Default, Serialize, Deserialize)]
pub struct InstanceInfo {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub settings: Option<crate::config::InstanceSettings>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrashEntry {
    pub token: String,
    pub id: String,
    pub label: String,
    pub removed_at: u64,
}
pub fn names(config: &Config) -> HashMap<String, String> {
    backend::installed(config)
        .into_iter()
        .filter_map(|id| {
            let path = config
                .game_dir
                .join("versions")
                .join(&id)
                .join("tui-mc-instance.json");
            let bytes = fs::read(path).ok()?;
            let info: InstanceInfo = serde_json::from_slice(&bytes).ok()?;
            if info.display_name.is_empty() {
                None
            } else {
                Some((id, info.display_name))
            }
        })
        .collect()
}
pub fn set_name(config: &Config, id: &str, name: &str) -> Result<()> {
    valid_id(id)?;
    if name.chars().any(char::is_control) || name.chars().count() > 80 {
        bail!("显示名称最多 80 个字符，不能包含控制字符");
    }
    let dir = config.root()?.join("versions").join(id);
    if !dir.join(format!("{id}.json")).is_file() {
        bail!("实例不存在");
    }
    let path = dir.join("tui-mc-instance.json");
    let mut info = if path.exists() {
        serde_json::from_slice::<InstanceInfo>(&fs::read(&path)?)?
    } else {
        InstanceInfo::default()
    };
    info.display_name = name.trim().into();
    atomic_write(&path, &serde_json::to_vec_pretty(&info)?)
}
pub fn clear_settings(config: &Config, id: &str) -> Result<()> {
    valid_id(id)?;
    let path = config
        .root()?
        .join("versions")
        .join(id)
        .join("tui-mc-instance.json");
    if !path.exists() {
        return Ok(());
    }
    let mut info: InstanceInfo = serde_json::from_slice(&fs::read(&path)?)?;
    info.settings = None;
    atomic_write(&path, &serde_json::to_vec_pretty(&info)?)
}
pub(crate) struct InstanceLock {
    _file: fs::File,
}
impl InstanceLock {
    pub(crate) fn acquire(dir: &Path) -> Result<Self> {
        Self::open(dir, false)
    }
    pub(crate) fn shared(dir: &Path) -> Result<Self> {
        Self::open(dir, true)
    }
    fn open(dir: &Path, shared: bool) -> Result<Self> {
        use sha1::{Digest, Sha1};
        let root = dir
            .parent()
            .and_then(Path::parent)
            .context("实例没有游戏根目录")?;
        let lock_dir = root.join("tui-mc-locks");
        fs::create_dir_all(&lock_dir)?;
        reject_link(&lock_dir)?;
        let id = dir.file_name().context("实例没有 ID")?.to_string_lossy();
        // Default macOS volumes also resolve differently-cased IDs to one directory.
        let id = if cfg!(any(windows, target_os = "macos")) {
            id.to_lowercase()
        } else {
            id.into_owned()
        };
        let key = format!("{:x}.lock", Sha1::digest(id.as_bytes()));
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_dir.join(key))?;
        let result = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        result
            .map_err(|_| anyhow::anyhow!("实例正在启动、运行或被其他操作使用，关闭游戏后重试"))?;
        Ok(Self { _file: file })
    }
}
pub fn dependents(config: &Config, id: &str) -> Result<Vec<String>> {
    let mut result = vec![];
    for other in backend::installed(config) {
        if other == id {
            continue;
        }
        let path = config
            .game_dir
            .join("versions")
            .join(&other)
            .join(format!("{other}.json"));
        let meta: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("实例 {other} 元数据损坏，无法确认依赖关系"))?;
        if meta["inheritsFrom"] == id || meta["jar"] == id {
            result.push(other);
        }
    }
    Ok(result)
}
pub fn trash(config: &Config, id: &str) -> Result<TrashEntry> {
    valid_id(id)?;
    let root = config.root()?;
    reject_link(&root.join("versions"))?;
    let dir = root.join("versions").join(id);
    if !dir.join(format!("{id}.json")).is_file() {
        bail!("实例不存在");
    }
    let children = dependents(config, id)?;
    if !children.is_empty() {
        bail!("还有实例依赖 {id}，请先处理：{}", children.join(", "));
    }
    let _running = InstanceLock::acquire(&dir)?;
    reject_link(&dir)?;
    let canonical = fs::canonicalize(&dir)?;
    let allowed = fs::canonicalize(root.join("versions"))?;
    if !canonical.starts_with(&allowed) || canonical == allowed {
        bail!("实例目录越过版本根目录");
    }
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let token = format!(
        "{millis}-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    let removed_at = (millis / 1000) as u64;
    let label = names(config).remove(id).unwrap_or_else(|| id.into());
    let entry = TrashEntry {
        token: token.clone(),
        id: id.into(),
        label,
        removed_at,
    };
    let trash_root = root.join("tui-mc-trash");
    fs::create_dir_all(&trash_root)?;
    reject_link(&trash_root)?;
    let dest = trash_root.join(token);
    fs::create_dir(&dest)?;
    atomic_write(
        &dest.join("entry.json"),
        &serde_json::to_vec_pretty(&entry)?,
    )?;
    if let Err(error) = fs::rename(&dir, dest.join("version")) {
        let _ = fs::remove_file(dest.join("entry.json"));
        let _ = fs::remove_dir(&dest);
        return Err(error).context("移至回收站失败，原实例保留");
    }
    Ok(entry)
}
pub fn trash_list(config: &Config) -> Result<Vec<TrashEntry>> {
    let root = config.root()?.join("tui-mc-trash");
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut result = vec![];
    for dir in fs::read_dir(root)? {
        let dir = dir?;
        if !dir.file_type()?.is_dir() {
            continue;
        }
        let path = dir.path();
        if let Ok(bytes) = fs::read(path.join("entry.json")) {
            if let Ok(entry) = serde_json::from_slice::<TrashEntry>(&bytes) {
                if entry.token == dir.file_name().to_string_lossy() && path.join("version").is_dir()
                {
                    result.push(entry);
                }
            }
        }
    }
    result.sort_by_key(|entry| std::cmp::Reverse(entry.removed_at));
    Ok(result)
}
pub fn restore(config: &Config, token: &str) -> Result<String> {
    valid_id(token)?;
    let root = config.root()?;
    reject_link(&root.join("tui-mc-trash"))?;
    let source = root.join("tui-mc-trash").join(token);
    reject_link(&source)?;
    let entry: TrashEntry = serde_json::from_slice(&fs::read(source.join("entry.json"))?)?;
    valid_id(&entry.id)?;
    if entry.token != token {
        bail!("回收站记录不一致");
    }
    let source_dir = source.join("version");
    reject_link(&source_dir)?;
    if !source_dir.join(format!("{}.json", entry.id)).is_file() {
        bail!("回收站实例数据不完整");
    }
    let dest = root.join("versions").join(&entry.id);
    if dest.exists() {
        bail!("同名实例已存在，请先处理当前实例，避免覆盖");
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    reject_link(dest.parent().unwrap())?;
    fs::rename(source_dir, dest)?;
    fs::remove_file(source.join("entry.json"))?;
    fs::remove_dir(source)?;
    Ok(entry.id)
}
pub fn data_files(
    root: &Path,
    include_personal: bool,
    control: &TaskControl,
) -> Result<Vec<(PathBuf, String)>> {
    let mut result = vec![];
    for name in DATA_DIRS {
        if !include_personal && matches!(*name, "saves" | "screenshots" | "journeymap" | "xaero") {
            continue;
        }
        let path = root.join(name);
        if path.exists() {
            walk(root, &path, 0, control, &mut result)?;
        }
    }
    for name in DATA_FILES {
        if !include_personal && matches!(*name, "servers.dat" | "servers.dat_old") {
            continue;
        }
        let path = root.join(name);
        if path.is_file() {
            reject_link(&path)?;
            result.push((path, (*name).into()));
        }
    }
    result.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(result)
}
fn walk(
    root: &Path,
    path: &Path,
    depth: usize,
    control: &TaskControl,
    result: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    control.check()?;
    if path
        .file_name()
        .is_some_and(|name| name == crate::download_lock::LOCK_DIR)
    {
        return Ok(());
    }
    if depth > 64 || result.len() > 100000 {
        bail!("实例文件层级或数量过大");
    }
    reject_link(path)?;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            walk(root, &entry?.path(), depth + 1, control, result)?;
        }
    } else {
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        safe_path(root, &relative)?;
        result.push((path.into(), relative));
    }
    Ok(())
}
fn reject_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("不处理符号链接或目录联接：{}", path.display());
        }
    }
    if metadata.file_type().is_symlink() {
        bail!("不处理符号链接：{}", path.display());
    }
    Ok(())
}
pub fn clone_instance(
    config: &Config,
    id: &str,
    target: &str,
    control: &TaskControl,
) -> Result<()> {
    valid_id(id)?;
    valid_id(target)?;
    if id.eq_ignore_ascii_case(target) {
        bail!("新实例 ID 必须不同");
    }
    let root = config.root()?;
    version::resolve(&root, id)?;
    let dest = root.join("versions").join(target);
    if dest.exists() {
        let pending = fs::read(dest.join("tui-mc-clone-pending.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        if dest.join(format!("{target}.json")).exists()
            || pending.is_none_or(|value| value["source"] != id)
        {
            bail!("目标实例已存在");
        }
    }
    let _running = InstanceLock::acquire(&root.join("versions").join(id))?;
    let source = config.effective(id)?.instance_dir(id)?;
    let files = data_files(&source, true, control)?;
    fs::create_dir_all(&dest)?;
    atomic_write(
        &dest.join("tui-mc-clone-pending.json"),
        &serde_json::to_vec(&json!({"source":id}))?,
    )?;
    for (source, relative) in files {
        control.check()?;
        crate::storage::atomic_copy(&safe_path(&dest, &relative)?, &mut fs::File::open(source)?)?;
    }
    let effective = config.effective(id)?;
    crate::pack_state::clone_receipt(config, id, target)?;
    let inventory = source.join(crate::modrinth::INVENTORY);
    if inventory.is_file() {
        crate::storage::atomic_copy(
            &dest.join(crate::modrinth::INVENTORY),
            &mut fs::File::open(inventory)?,
        )?;
    }
    let settings = crate::config::InstanceSettings {
        java: Some(effective.java),
        memory_mb: Some(effective.memory_mb),
        isolated: Some(true),
        jvm_args: Some(effective.jvm_args),
        game_args: Some(effective.game_args),
        fullscreen: Some(effective.fullscreen),
        server: Some(effective.server),
        game_language: Some(effective.game_language),
        patch_natives: Some(effective.patch_natives),
    };
    atomic_write(
        &dest.join("tui-mc-instance.json"),
        &serde_json::to_vec_pretty(&InstanceInfo {
            display_name: target.into(),
            settings: Some(settings),
        })?,
    )?;
    atomic_write(
        &dest.join(format!("{target}.json")),
        &serde_json::to_vec_pretty(&json!({"id":target,"inheritsFrom":id,"tuiMcIsolated":true}))?,
    )?;
    fs::remove_file(dest.join("tui-mc-clone-pending.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependent_version_prevents_removal() {
        let root = std::env::temp_dir().join(format!("tui-instance-{}", std::process::id()));
        let config = Config {
            game_dir: root.clone(),
            ..Config::default()
        };
        for (id, parent) in [("base", None), ("child", Some("base"))] {
            let dir = root.join("versions").join(id);
            fs::create_dir_all(&dir).unwrap();
            atomic_write(
                &dir.join(format!("{id}.json")),
                &serde_json::to_vec(&json!({"id":id,"inheritsFrom":parent})).unwrap(),
            )
            .unwrap();
        }
        assert!(trash(&config, "base").is_err());
        let removed = trash(&config, "child").unwrap();
        assert!(!root.join("versions/child").exists());
        assert_eq!(restore(&config, &removed.token).unwrap(), "child");
        for id in ["base", "child"] {
            let dir = root.join("versions").join(id);
            fs::remove_file(dir.join(format!("{id}.json"))).unwrap();
            fs::remove_dir(dir).unwrap();
        }
        fs::remove_dir(root.join("versions")).unwrap();
        fs::remove_dir(root.join("tui-mc-trash")).unwrap();
        for file in fs::read_dir(root.join("tui-mc-locks")).unwrap() {
            fs::remove_file(file.unwrap().path()).unwrap();
        }
        fs::remove_dir(root.join("tui-mc-locks")).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
