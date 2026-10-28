use crate::{
    download::valid_file_with_control,
    metadata::{safe_path, valid_id},
    modrinth::{InstalledMod, Inventory, INVENTORY},
    storage::atomic_write,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModFileState {
    Enabled,
    Disabled,
}
impl ModFileState {
    pub(crate) fn enabled(self) -> bool {
        self == Self::Enabled
    }
    pub(crate) fn destination(self, mods: &Path, filename: &str) -> Result<PathBuf> {
        safe_path(
            mods,
            &if self.enabled() {
                filename.into()
            } else {
                format!("{filename}.disabled")
            },
        )
    }
}

/// Activation is derived before replacement, independently of a version's filename.
pub(crate) struct ModInstallState {
    mods: PathBuf,
    previous: HashMap<String, String>,
}
impl ModInstallState {
    pub(crate) fn new(game_dir: &Path, current: &Inventory) -> Self {
        Self {
            mods: game_dir.join("mods"),
            previous: current
                .entries
                .iter()
                .map(|(project, record)| (project.clone(), record.filename.clone()))
                .collect(),
        }
    }
    fn existing(&self, filename: &str) -> Result<Option<ModFileState>> {
        valid_id(filename)?;
        let enabled = safe_path(&self.mods, filename)?;
        let disabled = safe_path(&self.mods, &format!("{filename}.disabled"))?;
        let regular_file = |path: &Path| -> Result<bool> {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("无法检查 Mod 文件：{}", path.display()))
                }
            };
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    bail!("Mod 目标是链接或重解析点，请先手动处理：{}", path.display());
                }
            }
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("Mod 目标不是普通文件，请先手动处理：{}", path.display());
            }
            Ok(true)
        };
        match (regular_file(&enabled)?, regular_file(&disabled)?) {
            (true, true) => bail!("Mod 同时存在启用和禁用副本，请先处理：{filename}"),
            (true, false) => Ok(Some(ModFileState::Enabled)),
            (false, true) => Ok(Some(ModFileState::Disabled)),
            _ => Ok(None),
        }
    }
    pub(crate) fn state(&self, project: &str, filename: &str) -> Result<ModFileState> {
        let target = self.existing(filename)?;
        if let Some(old) = self.previous.get(project) {
            if let Some(state) = self.existing(old)? {
                if old != filename && target.is_some_and(|target| target != state) {
                    bail!("新 Mod 文件名与既有启停状态冲突：{filename}");
                }
                return Ok(state);
            }
        }
        Ok(target.unwrap_or(ModFileState::Enabled))
    }
}

pub(crate) struct ModInstallTransaction<'a> {
    dir: &'a Path,
    staging: &'a Path,
    token: &'a str,
    records: &'a [(InstalledMod, String)],
    current: &'a Inventory,
    control: &'a crate::task::TaskControl,
}
impl<'a> ModInstallTransaction<'a> {
    pub(crate) fn new(
        dir: &'a Path,
        staging: &'a Path,
        token: &'a str,
        records: &'a [(InstalledMod, String)],
        current: &'a Inventory,
        control: &'a crate::task::TaskControl,
    ) -> Self {
        Self {
            dir,
            staging,
            token,
            records,
            current,
            control,
        }
    }
}

fn snapshot(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("读取安装前记录失败：{}", path.display())),
    }
}

impl ModInstallTransaction<'_> {
    pub(crate) fn commit(&self) -> Result<()> {
        self.commit_with_verifier(|path, record| {
            valid_file_with_control(path, Some(&record.sha1), Some(record.size), self.control)
        })
    }
    fn commit_with_verifier(
        &self,
        mut verify: impl FnMut(&Path, &InstalledMod) -> Result<bool>,
    ) -> Result<()> {
        let Self {
            dir,
            staging,
            token,
            records,
            current,
            control,
        } = *self;
        control.check()?;
        valid_id(token)?;
        let mods = dir.join("mods");
        fs::create_dir_all(&mods)?;
        let backup = dir.join("tui-mc-mod-backups").join(token);
        fs::create_dir_all(backup.parent().context("Mod 备份没有父目录")?)?;
        fs::create_dir(&backup).context("Mod 备份目录已存在或无法创建，不会覆盖历史备份")?;
        let old_index = snapshot(&dir.join(INVENTORY))?;
        let receipt_path = dir.join(crate::pack_state::RECEIPT);
        let old_receipt = snapshot(&receipt_path)?;
        let receipt: Option<crate::pack_state::Receipt> = old_receipt
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()?;
        let activation = ModInstallState::new(dir, current);
        for (record, _) in records {
            activation.state(&record.project, &record.filename)?;
        }
        let mut moved: Vec<(PathBuf, PathBuf)> = vec![];
        let mut created = vec![];
        let mut next = Inventory {
            entries: current.entries.clone(),
        };
        let result = (|| -> Result<()> {
            for (record, _) in records {
                control.check()?;
                valid_id(&record.filename)?;
                let dest = activation
                    .state(&record.project, &record.filename)?
                    .destination(&mods, &record.filename)?;
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
                    if verify(&dest, record)? {
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
                    let saved = safe_path(
                        &backup,
                        &dest
                            .file_name()
                            .context("Mod 没有文件名")?
                            .to_string_lossy(),
                    )?;
                    fs::rename(&dest, &saved)?;
                    moved.push((saved, dest.clone()));
                }
                let staged = safe_path(staging, &record.filename)?;
                if !verify(&staged, record)? {
                    bail!("Mod 暂存文件大小或 SHA-1 校验失败：{}", record.filename);
                }
                control.check()?;
                fs::rename(&staged, &dest)?;
                created.push((dest, staged));
                next.entries.insert(record.project.clone(), record.clone());
            }
            // Cancellation before metadata commit uses the same rollback as I/O failures.
            // Once committing snapshots, finish the transaction without pausing halfway.
            control.check()?;
            atomic_write(
                &backup.join("previous-inventory.json"),
                &serde_json::to_vec_pretty(current)?,
            )?;
            atomic_write(&dir.join(INVENTORY), &serde_json::to_vec_pretty(&next)?)?;
            if let Some(mut receipt) = receipt {
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
            let mut rollback_errors = vec![];
            for (path, staged) in created.into_iter().rev() {
                // Preserve downloaded input for recovery when its original slot is free.
                // Never overwrite an independently recreated staging file.
                let restore = (|| -> std::io::Result<()> {
                    if staged.try_exists()? {
                        fs::remove_file(&path)
                    } else {
                        fs::rename(&path, &staged)
                    }
                })();
                if let Err(error) = restore {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        rollback_errors.push(format!("退回暂存文件 {}：{error}", path.display()));
                    }
                }
            }
            for (saved, original) in moved.into_iter().rev() {
                if let Err(error) = fs::rename(&saved, &original) {
                    rollback_errors.push(format!("恢复 {}：{error}", original.display()));
                }
            }
            for (path, bytes) in [
                (dir.join(INVENTORY), old_index),
                (receipt_path, old_receipt),
            ] {
                match bytes {
                    Some(bytes) => {
                        if let Err(error) = atomic_write(&path, &bytes) {
                            rollback_errors.push(format!("恢复记录 {}：{error:#}", path.display()));
                        }
                    }
                    None => {
                        if let Err(error) = fs::remove_file(&path) {
                            if error.kind() != std::io::ErrorKind::NotFound {
                                rollback_errors
                                    .push(format!("移除记录 {}：{error}", path.display()));
                            }
                        }
                    }
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "Mod 安装失败，已尝试恢复原文件；备份位置：{}{}",
                    backup.display(),
                    if rollback_errors.is_empty() {
                        String::new()
                    } else {
                        format!("\n回滚未完全成功：\n{}", rollback_errors.join("\n"))
                    }
                )
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};
    #[test]
    #[ignore = "requires filesystem symbolic link permission"]
    fn linked_and_dangling_mods_are_not_treated_as_replaceable_files() {
        let workspace = crate::test_support::Workspace::new();
        let dir = &workspace.path;
        fs::create_dir(dir.join("mods")).unwrap();
        let target = dir.join("shared.jar");
        fs::write(&target, b"shared").unwrap();
        for name in ["linked.jar", "disabled.jar.disabled"] {
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(&target, dir.join("mods").join(name)).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, dir.join("mods").join(name)).unwrap();
        }
        let state = ModInstallState::new(dir, &Inventory::default());
        for dangling in [false, true] {
            if dangling {
                fs::remove_file(&target).unwrap();
            }
            for name in ["linked.jar", "disabled.jar"] {
                assert!(state.state("project", name).is_err());
            }
        }
        assert!(fs::symlink_metadata(dir.join("mods/linked.jar"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
    fn record(project: &str, filename: &str, bytes: &[u8]) -> InstalledMod {
        InstalledMod {
            project: project.into(),
            version: "version".into(),
            version_number: "1".into(),
            filename: filename.into(),
            sha1: format!("{:x}", Sha1::digest(bytes)),
            size: bytes.len() as u64,
            published: String::new(),
        }
    }
    fn prepare(dir: &Path) -> Inventory {
        fs::create_dir(dir.join("mods")).unwrap();
        fs::write(dir.join("mods/old.jar.disabled"), b"old").unwrap();
        let current = Inventory {
            entries: std::collections::BTreeMap::from([(
                "a".into(),
                record("a", "old.jar", b"old"),
            )]),
        };
        fs::write(dir.join(INVENTORY), serde_json::to_vec(&current).unwrap()).unwrap();
        let receipt = crate::pack_state::Receipt {
            schema: 1,
            instance: "game".into(),
            archive_sha1: "0".repeat(40),
            format: "modrinth".into(),
            files: vec![crate::pack_state::TrackedFile {
                path: "mods/old.jar".into(),
                sha1: current.entries["a"].sha1.clone(),
                size: 3,
                urls: vec!["https://example.com/old.jar".into()],
                member: None,
            }],
        };
        fs::write(
            dir.join(crate::pack_state::RECEIPT),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        current
    }
    #[test]
    fn updating_a_renamed_mod_keeps_it_disabled_and_updates_the_receipt() {
        let workspace = crate::test_support::Workspace::new();
        let dir = &workspace.path;
        let current = prepare(dir);
        let staging = dir.join("staging");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("new.jar"), b"new").unwrap();
        let records = [(
            record("a", "new.jar", b"new"),
            "https://example.com/new.jar".into(),
        )];
        ModInstallTransaction::new(
            dir,
            &staging,
            "update",
            &records,
            &current,
            &crate::task::TaskControl::default(),
        )
        .commit()
        .unwrap();
        assert_eq!(fs::read(dir.join("mods/new.jar.disabled")).unwrap(), b"new");
        assert!(!dir.join("mods/new.jar").exists());
        assert!(!dir.join("mods/old.jar.disabled").exists());
        assert_eq!(
            fs::read(dir.join("tui-mc-mod-backups/update/old.jar.disabled")).unwrap(),
            b"old"
        );
        let receipt: crate::pack_state::Receipt =
            serde_json::from_slice(&fs::read(dir.join(crate::pack_state::RECEIPT)).unwrap())
                .unwrap();
        assert_eq!(receipt.files.len(), 1);
        assert_eq!(receipt.files[0].path, "mods/new.jar");
    }
    #[test]
    fn same_version_disabled_reinstall_needs_no_staged_file() {
        let workspace = crate::test_support::Workspace::new();
        let dir = &workspace.path;
        let current = prepare(dir);
        let records = [(
            current.entries["a"].clone(),
            "https://example.com/old.jar".into(),
        )];
        ModInstallTransaction::new(
            dir,
            &dir.join("no-staging"),
            "reuse",
            &records,
            &current,
            &crate::task::TaskControl::default(),
        )
        .commit()
        .unwrap();
        assert_eq!(fs::read(dir.join("mods/old.jar.disabled")).unwrap(), b"old");
        assert!(!dir.join("mods/old.jar").exists());
    }
    #[test]
    fn ambiguous_activation_is_rejected_before_file_replacement() {
        let workspace = crate::test_support::Workspace::new();
        let dir = &workspace.path;
        let current = prepare(dir);
        fs::write(dir.join("mods/old.jar"), b"other").unwrap();
        let index = fs::read(dir.join(INVENTORY)).unwrap();
        let records = [(
            record("a", "new.jar", b"new"),
            "https://example.com/new.jar".into(),
        )];
        assert!(ModInstallTransaction::new(
            dir,
            &dir.join("no-staging"),
            "ambiguous",
            &records,
            &current,
            &crate::task::TaskControl::default()
        )
        .commit()
        .is_err());
        assert_eq!(fs::read(dir.join("mods/old.jar.disabled")).unwrap(), b"old");
        assert_eq!(fs::read(dir.join("mods/old.jar")).unwrap(), b"other");
        assert_eq!(fs::read(dir.join(INVENTORY)).unwrap(), index);
    }
    #[test]
    fn cancellation_after_first_replacement_restores_files_and_records() {
        let workspace = crate::test_support::Workspace::new();
        let dir = &workspace.path;
        let current = prepare(dir);
        let staging = dir.join("staging");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("new.jar"), b"new").unwrap();
        fs::write(staging.join("second.jar"), b"second").unwrap();
        let index = fs::read(dir.join(INVENTORY)).unwrap();
        let receipt = fs::read(dir.join(crate::pack_state::RECEIPT)).unwrap();
        let records = [
            (
                record("a", "new.jar", b"new"),
                "https://example.com/a".into(),
            ),
            (
                record("b", "second.jar", b"second"),
                "https://example.com/b".into(),
            ),
        ];
        let control = crate::task::TaskControl::default();
        let mut cancelled = false;
        let error = ModInstallTransaction::new(
            dir,
            &staging,
            "cancel-rollback",
            &records,
            &current,
            &control,
        )
        .commit_with_verifier(|path, record| {
            if record.project == "b" {
                assert_eq!(fs::read(dir.join("mods/new.jar.disabled")).unwrap(), b"new");
                assert!(!dir.join("mods/old.jar.disabled").exists());
                cancelled = true;
                control.cancel();
            }
            valid_file_with_control(path, Some(&record.sha1), Some(record.size), &control)
        })
        .unwrap_err();
        assert!(cancelled);
        let message = format!("{error:#}");
        assert!(message.contains("取消"), "{message}");
        assert!(!message.contains("回滚未完全成功"), "{message}");
        assert_eq!(fs::read(dir.join("mods/old.jar.disabled")).unwrap(), b"old");
        assert!(!dir.join("mods/new.jar.disabled").exists());
        assert!(!dir.join("mods/second.jar").exists());
        assert_eq!(fs::read(staging.join("second.jar")).unwrap(), b"second");
        assert_eq!(fs::read(staging.join("new.jar")).unwrap(), b"new");
        assert_eq!(fs::read(dir.join(INVENTORY)).unwrap(), index);
        assert_eq!(
            fs::read(dir.join(crate::pack_state::RECEIPT)).unwrap(),
            receipt
        );
        ModInstallTransaction::new(
            dir,
            &staging,
            "retry-after-cancel",
            &records,
            &current,
            &crate::task::TaskControl::default(),
        )
        .commit()
        .unwrap();
        assert_eq!(fs::read(dir.join("mods/new.jar.disabled")).unwrap(), b"new");
        assert_eq!(fs::read(dir.join("mods/second.jar")).unwrap(), b"second");
        assert!(!staging.join("new.jar").exists());
        assert!(!staging.join("second.jar").exists());
    }
    #[test]
    fn later_failure_rolls_back_disabled_files_and_both_metadata_snapshots() {
        let workspace = crate::test_support::Workspace::new();
        let dir = &workspace.path;
        let current = prepare(dir);
        fs::write(dir.join("mods/manual.jar"), b"manual").unwrap();
        let staging = dir.join("staging");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("new.jar"), b"new").unwrap();
        let index = fs::read(dir.join(INVENTORY)).unwrap();
        let receipt = fs::read(dir.join(crate::pack_state::RECEIPT)).unwrap();
        let records = [
            (
                record("a", "new.jar", b"new"),
                "https://example.com/a".into(),
            ),
            (
                record("b", "manual.jar", b"new"),
                "https://example.com/b".into(),
            ),
        ];
        let error = ModInstallTransaction::new(
            dir,
            &staging,
            "rollback",
            &records,
            &current,
            &crate::task::TaskControl::default(),
        )
        .commit()
        .unwrap_err();
        assert!(format!("{error:#}").contains("同名文件已存在"));
        assert_eq!(fs::read(dir.join("mods/old.jar.disabled")).unwrap(), b"old");
        assert!(!dir.join("mods/new.jar.disabled").exists());
        assert_eq!(fs::read(dir.join("mods/manual.jar")).unwrap(), b"manual");
        assert_eq!(fs::read(staging.join("new.jar")).unwrap(), b"new");
        assert_eq!(fs::read(dir.join(INVENTORY)).unwrap(), index);
        assert_eq!(
            fs::read(dir.join(crate::pack_state::RECEIPT)).unwrap(),
            receipt
        );
    }
}
