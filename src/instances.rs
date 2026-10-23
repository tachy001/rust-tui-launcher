use crate::{
    backend,
    config::Config,
    metadata::{safe_path, valid_id},
    storage::atomic_write,
    task::TaskControl,
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
pub struct RestoreOutcome {
    pub id: String,
    pub cleanup_warning: Option<String>,
}
impl RestoreOutcome {
    fn committed(id: String, source: &Path) -> Self {
        // The directory move has committed. Cleanup must not turn success into
        // an error or roll the player's restored files back into the trash.
        let cleanup =
            fs::remove_file(source.join("entry.json")).and_then(|()| fs::remove_dir(source));
        Self {
            id,
            cleanup_warning: cleanup.err().map(|error| {
                format!(
                    "实例已恢复；回收站残留记录清理失败：{}（{}）。残留目录不是待恢复实例，请勿移动或覆盖已恢复的数据。",
                    source.display(), error
                )
            }),
        }
    }
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
    let mut editor = InstanceMetadataEditor::begin(config, id)?;
    editor.value["display_name"] = name.trim().into();
    editor.commit()
}
/// Serializes edits with installs, game sessions, cloning and trash operations.
struct InstanceMetadataEditor {
    _mutation: crate::instance_service::InstanceMutation,
    path: PathBuf,
    original: Option<Vec<u8>>,
    value: serde_json::Value,
}
impl InstanceMetadataEditor {
    fn begin(config: &Config, id: &str) -> Result<Self> {
        valid_id(id)?;
        let root = config.root()?;
        reject_link(&root.join("versions"))?;
        let mutation = crate::instance_service::InstanceMutation::begin(&root, id)?;
        let directory = root.join("versions").join(id);
        reject_link(&directory)?;
        let profile = directory.join(format!("{id}.json"));
        reject_link(&profile)?;
        if !profile.is_file() {
            bail!("实例不存在");
        }
        let path = directory.join("tui-mc-instance.json");
        let original = match fs::symlink_metadata(&path) {
            Ok(_) => {
                reject_link(&path)?;
                Some(fs::read(&path)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let value = match &original {
            Some(bytes) => {
                // Validate known fields, but preserve fields from newer versions.
                let _: InstanceInfo = serde_json::from_slice(bytes)?;
                let value: serde_json::Value = serde_json::from_slice(bytes)?;
                if !value.is_object() {
                    bail!("实例设置必须为 JSON 对象");
                }
                value
            }
            None => json!({}),
        };
        Ok(Self {
            _mutation: mutation,
            path,
            original,
            value,
        })
    }
    fn commit(&self) -> Result<()> {
        atomic_write(&self.path, &serde_json::to_vec_pretty(&self.value)?)
    }
    fn rollback(&self) -> Result<()> {
        if let Some(bytes) = &self.original {
            atomic_write(&self.path, bytes)?;
        }
        Ok(())
    }
}
pub(crate) fn save_settings(config: &Config, id: &str) -> Result<()> {
    let _editor = InstanceMetadataEditor::begin(config, id)?;
    config.effective(id)?.validate()?;
    config.save()
}
pub(crate) fn reset_settings(config: &mut Config, id: &str) -> Result<()> {
    reset_settings_with(config, id, Config::save)
}
fn reset_settings_with(
    config: &mut Config,
    id: &str,
    persist: impl FnOnce(&Config) -> Result<()>,
) -> Result<()> {
    let mut editor = InstanceMetadataEditor::begin(config, id)?;
    let mut candidate = config.clone();
    candidate.instances.remove(id);
    if editor.original.is_some() {
        editor.value["settings"] = serde_json::Value::Null;
        editor.commit()?;
    }
    if let Err(error) = persist(&candidate) {
        if let Err(rollback) = editor.rollback() {
            bail!("保存全局配置失败：{error:#}；恢复原实例设置也失败：{rollback:#}。请重新加载配置并检查 {}", editor.path.display());
        }
        return Err(error).context("恢复全局设置失败，原实例设置已保留");
    }
    *config = candidate;
    Ok(())
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
    let _mutation = crate::instance_service::InstanceMutation::begin(&root, id)?;
    if !dir.join(format!("{id}.json")).is_file() {
        bail!("实例不存在");
    }
    let children = dependents(config, id)?;
    if !children.is_empty() {
        bail!("还有实例依赖 {id}，请先处理：{}", children.join(", "));
    }
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
pub fn restore(config: &Config, token: &str) -> Result<RestoreOutcome> {
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
    if fs::symlink_metadata(&dest).is_ok() {
        bail!("同名实例已存在，请先处理当前实例，避免覆盖");
    }
    let _mutation = crate::instance_service::InstanceMutation::begin(&root, &entry.id)?;
    if fs::symlink_metadata(&dest).is_ok() {
        bail!("同名实例已存在，请先处理当前实例，避免覆盖");
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    reject_link(dest.parent().unwrap())?;
    crate::publication_service::NoReplacePublisher
        .publish(&source_dir, &dest)
        .context("无法恢复实例；目标可能已存在，或文件系统不支持无覆盖移动")?;
    Ok(RestoreOutcome::committed(entry.id, &source))
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
    if path.file_name().is_some_and(|name| {
        name == crate::download_lock::LOCK_DIR
            || name == crate::loader_profile_service::JOURNAL
            || name == crate::loader_profile_service::REJECTED
    }) {
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
    control.check()?;
    valid_id(id)?;
    valid_id(target)?;
    if id.eq_ignore_ascii_case(target) {
        bail!("新实例 ID 必须不同");
    }
    let root = config.root()?;
    let dest = root.join("versions").join(target);
    if fs::symlink_metadata(&dest).is_ok() {
        reject_link(&dest)?;
    }
    let _target = crate::instance_service::InstanceMutation::begin(&root, target)?;
    let mut source_session = crate::instance_service::InstanceLaunchSession::begin(&root, id)?;
    let resolved = source_session.resolve(id)?;
    let effective = config.effective(id)?;
    let source = effective.instance_dir(id)?;
    let _game = crate::download_lock::DownloadLease::try_acquire(&source.join("options.txt"))?;
    let mut files = data_files(&source, true, control)?;
    for (path, name) in [
        (
            source.join(crate::modrinth::INVENTORY),
            crate::modrinth::INVENTORY,
        ),
        (
            root.join("versions")
                .join(id)
                .join(crate::pack_state::RECEIPT),
            crate::pack_state::RECEIPT,
        ),
    ] {
        if path.is_file() {
            reject_link(&path)?;
            files.push((path, name.into()));
        }
    }
    let snapshot = CloneSnapshot::capture(&resolved.meta, &effective, &files, control)?;
    if dest.exists() {
        let pending = fs::read(dest.join("tui-mc-clone-pending.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        if dest.join(format!("{target}.json")).exists()
            || pending.is_none_or(|value| {
                value["schema"] != 1
                    || value["source"] != id
                    || value["target"] != target
                    || value["snapshot"] != snapshot.fingerprint
            })
        {
            bail!("目标实例已存在，或复制源快照已变化 / 旧记录缺少快照；原目标已保留，请换用新实例 ID");
        }
    }
    fs::create_dir_all(&dest)?;
    atomic_write(
        &dest.join("tui-mc-clone-pending.json"),
        &serde_json::to_vec(
            &json!({"schema":1,"source":id,"target":target,"snapshot":snapshot.fingerprint}),
        )?,
    )?;
    for (source, relative) in files {
        control.check()?;
        crate::storage::atomic_copy_controlled(
            &safe_path(&dest, &relative)?,
            &mut fs::File::open(source)?,
            control,
        )?;
    }
    crate::pack_state::clone_receipt(config, id, target)?;
    atomic_write(
        &dest.join("tui-mc-instance.json"),
        &serde_json::to_vec_pretty(&InstanceInfo {
            display_name: target.into(),
            settings: Some(snapshot.settings),
        })?,
    )?;
    atomic_write(
        &dest.join(format!("{target}.json")),
        &serde_json::to_vec_pretty(&json!({"id":target,"inheritsFrom":id,"tuiMcIsolated":true}))?,
    )?;
    fs::remove_file(dest.join("tui-mc-clone-pending.json"))?;
    Ok(())
}

struct CloneSnapshot {
    fingerprint: String,
    settings: crate::config::InstanceSettings,
}
impl CloneSnapshot {
    fn capture(
        metadata: &serde_json::Value,
        effective: &Config,
        files: &[(PathBuf, String)],
        control: &TaskControl,
    ) -> Result<Self> {
        use sha1::{Digest, Sha1};
        let settings = crate::config::InstanceSettings {
            java: Some(effective.java.clone()),
            memory_mb: Some(effective.memory_mb),
            isolated: Some(true),
            jvm_args: Some(effective.jvm_args.clone()),
            game_args: Some(effective.game_args.clone()),
            fullscreen: Some(effective.fullscreen),
            server: Some(effective.server.clone()),
            quick_play_world: Some(effective.quick_play_world.clone()),
            game_language: Some(effective.game_language.clone()),
            patch_natives: Some(effective.patch_natives),
        };
        let mut records = vec![];
        for (source, relative) in files {
            control.check()?;
            let metadata = fs::metadata(source)?;
            let modified = match metadata.modified()?.duration_since(UNIX_EPOCH) {
                Ok(duration) => format!("+{}", duration.as_nanos()),
                Err(error) => format!("-{}", error.duration().as_nanos()),
            };
            records.push((relative.clone(), metadata.len(), modified));
        }
        records.sort();
        let bytes = serde_json::to_vec(&(metadata, &settings, records))?;
        Ok(Self {
            fingerprint: format!("{:x}", Sha1::digest(bytes)),
            settings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> Config {
        let config = Config {
            game_dir: root.into(),
            ..Config::default()
        };
        let directory = root.join("versions/base");
        fs::create_dir_all(directory.join("saves/World")).unwrap();
        fs::write(
            directory.join("base.json"),
            br#"{"id":"base","mainClass":"example.Main","libraries":[]}"#,
        )
        .unwrap();
        fs::write(directory.join("options.txt"), b"original options").unwrap();
        fs::write(directory.join("saves/World/level.dat"), b"fixture").unwrap();
        config
    }
    #[test]
    fn metadata_edits_respect_running_instance_and_do_not_change_memory_on_failure() {
        let temp = crate::test_support::Workspace::new();
        let mut config = fixture(&temp.path);
        config.instances.insert(
            "base".into(),
            crate::config::InstanceSettings {
                memory_mb: Some(4096),
                ..Default::default()
            },
        );
        let root = config.root().unwrap();
        let _running =
            crate::instance_service::InstanceLaunchSession::begin(&root, "base").unwrap();
        assert!(set_name(&config, "base", "new name").is_err());
        assert!(save_settings(&config, "base").is_err());
        let mut called = false;
        assert!(reset_settings_with(&mut config, "base", |_| {
            called = true;
            Ok(())
        })
        .is_err());
        assert!(!called);
        assert_eq!(config.instances["base"].memory_mb, Some(4096));
        assert!(!root.join("versions/base/tui-mc-instance.json").exists());
    }
    #[test]
    fn metadata_edit_preserves_unknown_fields_and_reset_commits_both_settings_layers() {
        let temp = crate::test_support::Workspace::new();
        let mut config = fixture(&temp.path);
        let root = config.root().unwrap();
        let path = root.join("versions/base/tui-mc-instance.json");
        fs::write(
            &path,
            br#"{"display_name":"old","settings":{"memory_mb":3072},"future":{"keep":true}}"#,
        )
        .unwrap();
        config.instances.insert(
            "base".into(),
            crate::config::InstanceSettings {
                memory_mb: Some(4096),
                ..Default::default()
            },
        );
        set_name(&config, "base", " new name ").unwrap();
        let info: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(info["display_name"], "new name");
        assert_eq!(info["settings"]["memory_mb"], 3072);
        assert_eq!(info["future"]["keep"], true);
        reset_settings_with(&mut config, "base", |candidate| {
            assert!(!candidate.instances.contains_key("base"));
            assert_eq!(candidate.effective("base")?.memory_mb, candidate.memory_mb);
            assert!(InstanceLock::acquire(&root.join("versions/base")).is_err());
            Ok(())
        })
        .unwrap();
        assert!(!config.instances.contains_key("base"));
        let info: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(info["settings"].is_null());
        assert_eq!(info["display_name"], "new name");
        assert_eq!(info["future"]["keep"], true);
    }
    #[test]
    fn reset_failure_restores_exact_original_bytes_and_keeps_in_memory_overrides() {
        let temp = crate::test_support::Workspace::new();
        let mut config = fixture(&temp.path);
        let path = config
            .root()
            .unwrap()
            .join("versions/base/tui-mc-instance.json");
        let original = b"{ \"display_name\": \"original\", \"settings\": {\"memory_mb\": 3072}, \"future\": 1 }\n";
        fs::write(&path, original).unwrap();
        config.instances.insert(
            "base".into(),
            crate::config::InstanceSettings {
                memory_mb: Some(4096),
                ..Default::default()
            },
        );
        assert!(
            reset_settings_with(&mut config, "base", |_| anyhow::bail!("save unavailable"))
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(config.instances["base"].memory_mb, Some(4096));
        fs::remove_file(&path).unwrap();
        assert!(
            reset_settings_with(&mut config, "base", |_| anyhow::bail!("save unavailable"))
                .is_err()
        );
        assert!(!path.exists());
    }
    #[test]
    fn reset_reports_both_persistence_and_rollback_errors() {
        let temp = crate::test_support::Workspace::new();
        let mut config = fixture(&temp.path);
        let path = config
            .root()
            .unwrap()
            .join("versions/base/tui-mc-instance.json");
        fs::write(&path, br#"{"settings":{"memory_mb":3072}}"#).unwrap();
        let error = reset_settings_with(&mut config, "base", |_| {
            fs::remove_file(&path)?;
            fs::create_dir(&path)?;
            anyhow::bail!("fixture persistence failure")
        })
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("fixture persistence failure"));
        assert!(message.contains("恢复原实例设置也失败"));
        assert!(path.is_dir());
    }

    #[test]
    fn clone_reserves_target_and_shared_source_then_copies_worlds_and_settings() {
        let temp = crate::test_support::Workspace::new();
        let mut config = fixture(&temp.path);
        config
            .instances
            .entry("base".into())
            .or_default()
            .quick_play_world = Some("World".into());
        let root = config.root().unwrap();
        let target = root.join("versions/copy");
        let held = InstanceLock::acquire(&target).unwrap();
        assert!(clone_instance(&config, "base", "copy", &TaskControl::default()).is_err());
        assert!(!target.exists());
        drop(held);
        let source = config
            .effective("base")
            .unwrap()
            .instance_dir("base")
            .unwrap();
        let held = crate::download_lock::DownloadLease::acquire(
            &source.join("options.txt"),
            &TaskControl::default(),
            || {},
        )
        .unwrap();
        assert!(clone_instance(&config, "base", "copy", &TaskControl::default()).is_err());
        assert!(!target.exists());
        drop(held);
        clone_instance(&config, "base", "copy", &TaskControl::default()).unwrap();
        assert_eq!(
            fs::read(target.join("options.txt")).unwrap(),
            b"original options"
        );
        assert_eq!(
            fs::read(target.join("saves/World/level.dat")).unwrap(),
            b"fixture"
        );
        assert_eq!(config.effective("copy").unwrap().quick_play_world, "World");
        assert!(!target.join("tui-mc-clone-pending.json").exists());
        assert!(trash(&config, "base").is_err());
    }

    #[test]
    fn clone_resume_requires_unchanged_snapshot_and_preserves_stale_target() {
        let temp = crate::test_support::Workspace::new();
        let config = fixture(&temp.path);
        let root = config.root().unwrap();
        let source = config
            .effective("base")
            .unwrap()
            .instance_dir("base")
            .unwrap();
        let files = data_files(&source, true, &TaskControl::default()).unwrap();
        let meta = crate::version::resolve(&root, "base").unwrap().meta;
        let snapshot = CloneSnapshot::capture(
            &meta,
            &config.effective("base").unwrap(),
            &files,
            &TaskControl::default(),
        )
        .unwrap();
        for target in ["resume", "stale"] {
            let dest = root.join("versions").join(target);
            fs::create_dir_all(&dest).unwrap();
            fs::write(dest.join("options.txt"), b"partial copy").unwrap();
            atomic_write(&dest.join("tui-mc-clone-pending.json"), &serde_json::to_vec(&json!({"schema":1,"source":"base","target":target,"snapshot":snapshot.fingerprint})).unwrap()).unwrap();
        }
        clone_instance(&config, "base", "resume", &TaskControl::default()).unwrap();
        fs::write(
            source.join("options.txt"),
            b"changed source options, different snapshot",
        )
        .unwrap();
        assert!(clone_instance(&config, "base", "stale", &TaskControl::default()).is_err());
        assert_eq!(
            fs::read(root.join("versions/stale/options.txt")).unwrap(),
            b"partial copy"
        );
        assert!(!root.join("versions/stale/stale.json").exists());
        let cancelled = TaskControl::default();
        cancelled.cancel();
        assert!(clone_instance(&config, "base", "cancelled", &cancelled).is_err());
        assert!(!root.join("versions/cancelled").exists());
    }

    #[test]
    fn restore_respects_reserved_target_and_keeps_trash_until_it_can_publish() {
        let temp = crate::test_support::Workspace::new();
        let config = fixture(&temp.path);
        let removed = trash(&config, "base").unwrap();
        let root = config.root().unwrap();
        let held = InstanceLock::acquire(&root.join("versions/base")).unwrap();
        assert!(restore(&config, &removed.token).is_err());
        assert!(root
            .join("tui-mc-trash")
            .join(&removed.token)
            .join("version/base.json")
            .is_file());
        assert!(!root.join("versions/base").exists());
        drop(held);
        let restored = restore(&config, &removed.token).unwrap();
        assert_eq!(restored.id, "base");
        assert!(restored.cleanup_warning.is_none());
        assert_eq!(
            fs::read(root.join("versions/base/saves/World/level.dat")).unwrap(),
            b"fixture"
        );
    }
    #[test]
    fn committed_restore_reports_cleanup_warning_without_removing_foreign_files() {
        let temp = crate::test_support::Workspace::new();
        let config = fixture(&temp.path);
        let removed = trash(&config, "base").unwrap();
        let root = config.root().unwrap();
        let source = root.join("tui-mc-trash").join(&removed.token);
        fs::write(source.join("external-note.txt"), b"keep this file").unwrap();
        let restored = restore(&config, &removed.token).unwrap();
        assert_eq!(restored.id, "base");
        assert!(restored.cleanup_warning.unwrap().contains("清理失败"));
        assert_eq!(
            fs::read(root.join("versions/base/saves/World/level.dat")).unwrap(),
            b"fixture"
        );
        assert_eq!(
            fs::read(source.join("external-note.txt")).unwrap(),
            b"keep this file"
        );
        assert!(!source.join("version").exists());
        assert!(trash_list(&config).unwrap().is_empty());
        assert!(restore(&config, &removed.token).is_err());
        assert!(root.join("versions/base/base.json").is_file());
    }
    #[test]
    fn copying_inactive_sibling_keeps_parent_read_only_and_allows_other_sibling_session() {
        let temp = crate::test_support::Workspace::new();
        let config = fixture(&temp.path);
        let root = config.root().unwrap();
        for id in ["first", "second"] {
            let directory = root.join("versions").join(id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join(format!("{id}.json")),
                serde_json::to_vec(&json!({"id":id,"inheritsFrom":"base"})).unwrap(),
            )
            .unwrap();
        }
        let mut other =
            crate::instance_service::InstanceLaunchSession::begin(&root, "first").unwrap();
        other.resolve("first").unwrap();
        clone_instance(&config, "second", "copy", &TaskControl::default()).unwrap();
        assert_eq!(
            crate::version::resolve(&root, "copy").unwrap().meta["mainClass"],
            "example.Main"
        );
    }
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
        assert_eq!(restore(&config, &removed.token).unwrap().id, "child");
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
