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
    for entry in fs::read_dir(directory(config, id)?)? {
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
    if safe_path(&mods, &counterpart)?.exists() {
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

#[derive(Debug)]
pub struct RemovedMod {
    pub token: String,
    pub filename: String,
    modified: std::time::SystemTime,
}

pub fn removed(config: &Config, id: &str) -> Result<Vec<RemovedMod>> {
    let base = config
        .effective(id)?
        .instance_dir(id)?
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
    if safe_path(&mods, base)?.exists() || safe_path(&mods, &format!("{base}.disabled"))?.exists() {
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
    fs::rename(&source, &dest)?;
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
