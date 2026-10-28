use crate::{config::Config, metadata::safe_path};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct ModEntry {
    pub filename: String,
    pub enabled: bool,
    pub size: u64,
}
pub fn directory(config: &Config, id: &str) -> Result<PathBuf> {
    let dir = config.effective(id)?.instance_dir(id)?.join("mods");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}
pub fn list(config: &Config, id: &str) -> Result<Vec<ModEntry>> {
    let mut entries = vec![];
    let directory = config.effective(id)?.instance_path(id)?.join("mods");
    let files = match fs::read_dir(&directory) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("无法读取 Mod 目录：{}", directory.display()))
        }
    };
    for entry in files {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let filename = entry.file_name().to_string_lossy().to_string();
        let lower = filename.to_lowercase();
        if lower.ends_with(".jar") || lower.ends_with(".jar.disabled") {
            entries.push(ModEntry {
                enabled: lower.ends_with(".jar"),
                filename,
                size: entry.metadata()?.len(),
            });
        }
    }
    entries.sort_by_key(|entry| entry.filename.to_lowercase());
    Ok(entries)
}
pub fn toggle(config: &Config, id: &str, filename: &str) -> Result<()> {
    crate::local_mod_service::ModMutationSession::begin(config, id)?.toggle(filename)
}
pub fn import(config: &Config, id: &str, source: &Path) -> Result<()> {
    crate::local_mod_service::ModMutationSession::begin(config, id)?
        .import(source, &crate::task::TaskControl::default())
}

/// Removal is reversible: keep the file and original management records together.
pub fn remove(config: &Config, id: &str, filename: &str) -> Result<PathBuf> {
    let _session = crate::local_mod_service::ModMutationSession::begin(config, id)?;
    let mods = directory(config, id)?;
    let source = safe_path(&mods, filename)?;
    if !list(config, id)?
        .iter()
        .any(|entry| entry.filename == filename)
    {
        bail!("请选择现有 JAR Mod 文件");
    }
    let dir = mods.parent().context("Mod 目录没有父目录")?;
    let inventory_path = dir.join(crate::modrinth::INVENTORY);
    let receipt_path = dir.join(crate::pack_state::RECEIPT);
    let original_inventory = read_optional(&inventory_path)?;
    let original_receipt = read_optional(&receipt_path)?;
    let base = if filename.to_lowercase().ends_with(".disabled") {
        &filename[..filename.len() - ".disabled".len()]
    } else {
        filename
    };
    let counterpart = if base == filename {
        format!("{base}.disabled")
    } else {
        base.to_owned()
    };
    if occupied(&safe_path(&mods, &counterpart)?)? {
        bail!("同时存在启用和禁用副本，请先整理同名文件，避免移除后管理记录产生歧义");
    }
    let mut next_inventory = None;
    let mut next_receipt = None;
    if let Some(bytes) = &original_inventory {
        let mut inventory: crate::modrinth::Inventory = serde_json::from_slice(bytes)?;
        inventory
            .entries
            .retain(|_, record| !record.filename.eq_ignore_ascii_case(base));
        next_inventory = Some(serde_json::to_vec_pretty(&inventory)?);
    }
    if let Some(bytes) = &original_receipt {
        let mut receipt: crate::pack_state::Receipt = serde_json::from_slice(bytes)?;
        receipt.files.retain(|record| {
            !record.path.eq_ignore_ascii_case(&format!("mods/{base}"))
                && !record
                    .path
                    .eq_ignore_ascii_case(&format!("mods/{filename}"))
        });
        next_receipt = Some(serde_json::to_vec_pretty(&receipt)?);
    }
    let token = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let backup = dir.join("tui-mc-removed-mods").join(token);
    fs::create_dir_all(&backup)?;
    if let Some(bytes) = &original_inventory {
        crate::storage::atomic_write(&backup.join("previous-modrinth.json"), bytes)?;
    }
    if let Some(bytes) = &original_receipt {
        crate::storage::atomic_write(&backup.join("previous-pack-receipt.json"), bytes)?;
    }
    fs::rename(&source, backup.join(filename))?;
    let result = (|| -> Result<()> {
        if let Some(bytes) = &next_inventory {
            crate::storage::atomic_write(&inventory_path, bytes)?;
        }
        if let Some(bytes) = &next_receipt {
            crate::storage::atomic_write(&receipt_path, bytes)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<()> {
            if let Some(bytes) = &original_inventory {
                crate::storage::atomic_write(&inventory_path, bytes)?;
            }
            fs::rename(backup.join(filename), &source)?;
            Ok(())
        })();
        if let Err(rollback_error) = rollback {
            bail!(
                "移除失败：{error}；恢复失败：{rollback_error}；备份：{}",
                backup.display()
            );
        }
        return Err(error);
    }
    Ok(backup)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn occupied(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("无法检查 Mod 目标：{}", path.display())),
    }
}

fn restore_file(source: &Path, destination: &Path) -> Result<()> {
    // Unlike rename on Unix, hard_link atomically rejects an occupied name.
    fs::hard_link(source, destination)
        .context("无法恢复 Mod：目标已存在或文件系统不支持硬链接；备份已保留")?;
    if let Err(error) = fs::remove_file(source) {
        if let Err(rollback) = fs::remove_file(destination) {
            bail!("恢复 Mod 失败：{error}；撤回新名称失败：{rollback}；备份已保留");
        }
        return Err(error).context("无法移除备份中的原名称，恢复已撤回");
    }
    Ok(())
}

#[derive(Debug)]
pub struct RemovedMod {
    pub token: String,
    pub filename: String,
    modified: std::time::SystemTime,
}

pub fn removed(config: &Config, id: &str) -> Result<Vec<RemovedMod>> {
    let base = config
        .effective(id)?
        .instance_path(id)?
        .join("tui-mc-removed-mods");
    let entries = match fs::read_dir(base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    let mut result = vec![];
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            let name = file.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            if file.file_type()?.is_file()
                && (lower.ends_with(".jar") || lower.ends_with(".jar.disabled"))
            {
                result.push(RemovedMod {
                    token: entry.file_name().to_string_lossy().into_owned(),
                    filename: name,
                    modified: entry.metadata()?.modified()?,
                });
            }
        }
    }
    result.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.token.cmp(&a.token))
    });
    Ok(result)
}

pub fn restore(config: &Config, id: &str, token: &str) -> Result<String> {
    let _session = crate::local_mod_service::ModMutationSession::begin(config, id)?;
    crate::metadata::valid_id(token)?;
    let choices: Vec<_> = removed(config, id)?
        .into_iter()
        .filter(|entry| entry.token == token)
        .collect();
    if choices.len() != 1 {
        bail!("备份不存在、已恢复或包含多个 JAR，无法自动恢复");
    }
    let filename = &choices[0].filename;
    let mods = directory(config, id)?;
    let dir = mods.parent().context("Mod 目录没有父目录")?;
    let backup = safe_path(&dir.join("tui-mc-removed-mods"), token)?;
    let source = safe_path(&backup, filename)?;
    let dest = safe_path(&mods, filename)?;
    let base = if filename.to_lowercase().ends_with(".disabled") {
        &filename[..filename.len() - ".disabled".len()]
    } else {
        filename.as_str()
    };
    if occupied(&safe_path(&mods, base)?)?
        || occupied(&safe_path(&mods, &format!("{base}.disabled"))?)?
    {
        bail!("已有同名启用 / 禁用文件，不会覆盖，请先整理冲突文件");
    }
    let inventory_path = dir.join(crate::modrinth::INVENTORY);
    let receipt_path = dir.join(crate::pack_state::RECEIPT);
    let original_inventory = read_optional(&inventory_path)?;
    let original_receipt = read_optional(&receipt_path)?;
    let mut next_inventory = None;
    let mut next_receipt = None;
    if let Some(bytes) = read_optional(&backup.join("previous-modrinth.json"))? {
        let old: crate::modrinth::Inventory = serde_json::from_slice(&bytes)?;
        let mut current: crate::modrinth::Inventory = original_inventory
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()?
            .unwrap_or_default();
        for (project, record) in old.entries {
            if !record.filename.eq_ignore_ascii_case(base) {
                continue;
            }
            if current.entries.contains_key(&project)
                || current
                    .entries
                    .values()
                    .any(|other| other.filename.eq_ignore_ascii_case(base))
            {
                bail!("该项目已有新的安装记录，不能恢复旧版本");
            }
            current.entries.insert(project, record);
            next_inventory = Some(serde_json::to_vec_pretty(&current)?);
        }
    }
    if let Some(bytes) = read_optional(&backup.join("previous-pack-receipt.json"))? {
        let mut old: crate::pack_state::Receipt = serde_json::from_slice(&bytes)?;
        old.files.retain(|record| {
            record.path.eq_ignore_ascii_case(&format!("mods/{base}"))
                || record
                    .path
                    .eq_ignore_ascii_case(&format!("mods/{filename}"))
        });
        if !old.files.is_empty() {
            if let Some(bytes) = &original_receipt {
                let mut current: crate::pack_state::Receipt = serde_json::from_slice(bytes)?;
                if current.archive_sha1 != old.archive_sha1 {
                    bail!("整合包来源已经改变，无法合并原修复记录");
                }
                for record in old.files {
                    if current
                        .files
                        .iter()
                        .any(|other| other.path.eq_ignore_ascii_case(&record.path))
                    {
                        bail!("该文件已有新的整合包修复记录，拒绝覆盖");
                    }
                    current.files.push(record);
                }
                next_receipt = Some(serde_json::to_vec_pretty(&current)?);
            } else {
                next_receipt = Some(serde_json::to_vec_pretty(&old)?);
            }
        }
    }
    restore_file(&source, &dest)?;
    let result = (|| -> Result<()> {
        if let Some(bytes) = &next_inventory {
            crate::storage::atomic_write(&inventory_path, bytes)?;
        }
        if let Some(bytes) = &next_receipt {
            crate::storage::atomic_write(&receipt_path, bytes)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<()> {
            match &original_inventory {
                Some(bytes) => crate::storage::atomic_write(&inventory_path, bytes)?,
                None if inventory_path.exists() => fs::remove_file(&inventory_path)?,
                None => (),
            }
            fs::rename(&dest, &source)?;
            Ok(())
        })();
        if let Err(rollback_error) = rollback {
            bail!(
                "恢复失败：{error}；回滚失败：{rollback_error}；备份：{}",
                backup.display()
            );
        }
        return Err(error);
    }
    Ok(filename.clone())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restore_publication_rejects_a_target_created_after_preflight() {
        let temp = crate::test_support::Workspace::new();
        let source = temp.path.join("backup.jar");
        let destination = temp.path.join("restored.jar");
        fs::write(&source, b"backup").unwrap();
        assert!(!occupied(&destination).unwrap());
        fs::write(&destination, b"concurrent file").unwrap();
        assert!(restore_file(&source, &destination).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"backup");
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent file");
        fs::remove_file(&destination).unwrap();
        restore_file(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(destination).unwrap(), b"backup");
    }
    #[test]
    #[ignore = "requires permission to create filesystem symbolic links"]
    fn dangling_counterparts_block_removal_and_restore_without_changing_backup() {
        let temp = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: temp.path.clone(),
            isolated: false,
            ..Config::default()
        };
        let mods = directory(&config, "test").unwrap();
        let enabled = mods.join("a.jar");
        let disabled = mods.join("a.jar.disabled");
        let absent = temp.path.join("absent.jar");
        let link = |path: &Path| {
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(&absent, path).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&absent, path).unwrap();
        };
        fs::write(&enabled, b"original").unwrap();
        link(&disabled);
        assert!(remove(&config, "test", "a.jar").is_err());
        assert_eq!(fs::read(&enabled).unwrap(), b"original");
        assert_eq!(fs::read_link(&disabled).unwrap(), absent);
        fs::remove_file(&disabled).unwrap();
        let backup = remove(&config, "test", "a.jar").unwrap();
        let token = backup.file_name().unwrap().to_str().unwrap();
        for path in [&enabled, &disabled] {
            link(path);
            assert!(restore(&config, "test", token).is_err());
            assert_eq!(fs::read(backup.join("a.jar")).unwrap(), b"original");
            assert_eq!(fs::read_link(path).unwrap(), absent);
            fs::remove_file(path).unwrap();
        }
        assert_eq!(restore(&config, "test", token).unwrap(), "a.jar");
        assert_eq!(fs::read(enabled).unwrap(), b"original");
    }
    #[test]
    fn listing_does_not_create_missing_directories_or_hide_invalid_directory() {
        let temp = crate::test_support::Workspace::new();
        let root = temp.path.join("not-created");
        let config = Config {
            game_dir: root.clone(),
            isolated: false,
            ..Config::default()
        };
        assert!(list(&config, "test").unwrap().is_empty());
        assert!(!root.exists());
        fs::create_dir(&root).unwrap();
        fs::write(root.join("mods"), b"not a directory").unwrap();
        let error = list(&config, "test").unwrap_err();
        assert!(error.to_string().contains("mods"));
        assert_eq!(fs::read(root.join("mods")).unwrap(), b"not a directory");
    }
    #[test]
    fn removal_preserves_backup_and_detaches_repair_records() {
        let temp = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: temp.path.clone(),
            isolated: false,
            ..Config::default()
        };
        let dir = directory(&config, "test").unwrap();
        fs::write(dir.join("a.jar.disabled"), b"original").unwrap();
        fs::write(temp.path.join(crate::modrinth::INVENTORY), br#"{"entries":{"a":{"project":"a","version":"v1","version_number":"1","filename":"a.jar","sha1":"abc","size":8}}}"#).unwrap();
        fs::write(temp.path.join(crate::pack_state::RECEIPT), br#"{"schema":1,"instance":"test","archive_sha1":"abc","format":"mrpack","files":[{"path":"mods/a.jar","sha1":"abc","size":8},{"path":"config/keep.txt","sha1":"def","size":3}]}"#).unwrap();
        let backup = remove(&config, "test", "a.jar.disabled").unwrap();
        assert_eq!(
            fs::read(backup.join("a.jar.disabled")).unwrap(),
            b"original"
        );
        assert!(list(&config, "test").unwrap().is_empty());
        let inventory: crate::modrinth::Inventory =
            serde_json::from_slice(&fs::read(temp.path.join(crate::modrinth::INVENTORY)).unwrap())
                .unwrap();
        assert!(inventory.entries.is_empty());
        let receipt: crate::pack_state::Receipt =
            serde_json::from_slice(&fs::read(temp.path.join(crate::pack_state::RECEIPT)).unwrap())
                .unwrap();
        assert_eq!(receipt.files.len(), 1);
        assert_eq!(receipt.files[0].path, "config/keep.txt");
        assert!(backup.join("previous-modrinth.json").is_file());
        let token = backup.file_name().unwrap().to_str().unwrap();
        assert_eq!(removed(&config, "test").unwrap()[0].token, token);
        fs::write(dir.join("a.jar"), b"new manual").unwrap();
        assert!(restore(&config, "test", token).is_err());
        assert!(backup.join("a.jar.disabled").is_file());
        assert_eq!(fs::read(dir.join("a.jar")).unwrap(), b"new manual");
        fs::remove_file(dir.join("a.jar")).unwrap();
        fs::write(temp.path.join(crate::modrinth::INVENTORY), br#"{"entries":{"b":{"project":"b","version":"v2","version_number":"2","filename":"b.jar","sha1":"def","size":3}}}"#).unwrap();
        assert_eq!(restore(&config, "test", token).unwrap(), "a.jar.disabled");
        assert_eq!(fs::read(dir.join("a.jar.disabled")).unwrap(), b"original");
        assert!(!dir.join("a.jar").exists());
        assert!(removed(&config, "test").unwrap().is_empty());
        assert!(restore(&config, "test", token).is_err());
        let restored: crate::modrinth::Inventory =
            serde_json::from_slice(&fs::read(temp.path.join(crate::modrinth::INVENTORY)).unwrap())
                .unwrap();
        assert_eq!(restored.entries.len(), 2);
        assert_eq!(restored.entries["b"].version, "v2");
        let restored: crate::pack_state::Receipt =
            serde_json::from_slice(&fs::read(temp.path.join(crate::pack_state::RECEIPT)).unwrap())
                .unwrap();
        assert_eq!(restored.files.len(), 2);
        assert!(restored
            .files
            .iter()
            .any(|record| record.path == "config/keep.txt"));
        fs::write(dir.join("b.jar"), b"manual").unwrap();
        fs::write(temp.path.join(crate::pack_state::RECEIPT), b"broken").unwrap();
        assert!(remove(&config, "test", "b.jar").is_err());
        assert_eq!(fs::read(dir.join("b.jar")).unwrap(), b"manual");
    }
    #[test]
    fn toggle_roundtrip_preserves_content() {
        let workspace = crate::test_support::Workspace::new();
        let root = workspace.path.clone();
        let config = Config {
            game_dir: root.clone(),
            isolated: false,
            ..Config::default()
        };
        let dir = directory(&config, "test").unwrap();
        fs::write(dir.join("a.jar"), b"unchanged").unwrap();
        toggle(&config, "test", "a.jar").unwrap();
        assert!(!list(&config, "test").unwrap()[0].enabled);
        toggle(&config, "test", "a.jar.disabled").unwrap();
        assert_eq!(fs::read(dir.join("a.jar")).unwrap(), b"unchanged");
        assert!(toggle(&config, "test", "../a.jar").is_err());
        fs::write(dir.join("a.jar.disabled"), b"another").unwrap();
        assert!(toggle(&config, "test", "a.jar").is_err());
        fs::remove_file(dir.join("a.jar")).unwrap();
        fs::remove_file(dir.join("a.jar.disabled")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
