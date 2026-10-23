use crate::{instance_service::InstanceMutation, metadata::valid_id, storage::atomic_write};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) const JOURNAL: &str = "tui-mc-loader-profile-journal.json";
pub(crate) const REJECTED: &str = "tui-mc-loader-rejected";
const MAX_PROFILE: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    id: String,
    original: Option<String>,
    sha1: Option<String>,
    committed: bool,
    quiescent: bool,
}

fn read(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!("加载器恢复路径不是普通文件：{}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("读取加载器恢复文件失败"),
    }
    let mut bytes = vec![];
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("加载器恢复文件超过大小限制");
    }
    Ok(Some(bytes))
}

/// A durable transaction for the target version JSON, not for shared libraries.
pub(crate) struct LoaderProfileTransaction {
    directory: PathBuf,
    record: Record,
}
impl LoaderProfileTransaction {
    pub(crate) fn begin(root: &Path, id: &str, mutation: &InstanceMutation) -> Result<Self> {
        valid_id(id)?;
        mutation.require(root, id)?;
        let directory = root.join("versions").join(id);
        if Self::pending(root, id)? {
            bail!("已有未处理的加载器配置恢复记录，拒绝覆盖");
        }
        let original = read(&directory.join(format!("{id}.json")), MAX_PROFILE)?;
        let record = Record {
            schema: 1,
            id: id.into(),
            sha1: original
                .as_deref()
                .map(|bytes| format!("{:x}", Sha1::digest(bytes))),
            original: original.map(|bytes| STANDARD.encode(bytes)),
            committed: false,
            quiescent: true,
        };
        atomic_write(&directory.join(JOURNAL), &serde_json::to_vec(&record)?)?;
        Ok(Self { directory, record })
    }

    pub(crate) fn pending(root: &Path, id: &str) -> Result<bool> {
        valid_id(id)?;
        Ok(read(
            &root.join("versions").join(id).join(JOURNAL),
            24 * 1024 * 1024,
        )?
        .is_some())
    }

    pub(crate) fn commit(&mut self) -> Result<Option<String>> {
        if !self.record.quiescent {
            bail!("安装器退出尚未确认，不能提交实例配置");
        }
        self.record.committed = true;
        atomic_write(
            &self.directory.join(JOURNAL),
            &serde_json::to_vec(&self.record)?,
        )?;
        Ok(fs::remove_file(self.directory.join(JOURNAL))
            .err()
            .map(|error| format!("安装已提交，但加载器配置恢复记录未能清理：{error}")))
    }

    pub(crate) fn set_installer_active(&mut self, active: bool) -> Result<()> {
        self.record.quiescent = !active;
        atomic_write(
            &self.directory.join(JOURNAL),
            &serde_json::to_vec(&self.record)?,
        )
    }

    pub(crate) fn rollback(&self) -> Result<Option<String>> {
        let root = self
            .directory
            .parent()
            .and_then(Path::parent)
            .context("加载器恢复目录无效")?;
        Self::recover(root, &self.record.id)
    }

    /// Caller holds the instance's exclusive OS lock; never restore under a read lock.
    pub(crate) fn recover(root: &Path, id: &str) -> Result<Option<String>> {
        Self::recover_using(root, id, false)
    }

    /// Only exposed through an explicit user assertion that the installer stopped.
    pub(crate) fn recover_stopped(root: &Path, id: &str) -> Result<Option<String>> {
        valid_id(id)?;
        let _lock = crate::instances::InstanceLock::acquire(&root.join("versions").join(id))?;
        // Match the install path's lock order: instance, then shared directory.
        let _installation = crate::installation_service::LoaderInstallSession::for_recovery(root)?;
        Self::recover_using(root, id, true)
    }

    fn recover_using(
        root: &Path,
        id: &str,
        installer_confirmed_stopped: bool,
    ) -> Result<Option<String>> {
        valid_id(id)?;
        let directory = root.join("versions").join(id);
        let Some(bytes) = read(&directory.join(JOURNAL), 24 * 1024 * 1024)? else {
            return Ok(None);
        };
        let record: Record =
            serde_json::from_slice(&bytes).context("加载器配置恢复记录损坏，已保留现场")?;
        if record.schema != 1 || record.id != id {
            bail!("加载器配置恢复记录身份或版本不匹配");
        }
        let original = record
            .original
            .as_deref()
            .map(|value| STANDARD.decode(value))
            .transpose()
            .context("加载器原配置备份损坏")?;
        if original
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 > MAX_PROFILE)
            || original
                .as_deref()
                .map(|bytes| format!("{:x}", Sha1::digest(bytes)))
                != record.sha1
        {
            bail!("加载器原配置备份校验失败，已保留现场");
        }
        if record.committed {
            fs::remove_file(directory.join(JOURNAL))?;
            return Ok(Some("已清理成功安装后的加载器配置恢复记录".into()));
        }
        if !record.quiescent && !installer_confirmed_stopped {
            bail!("上次加载器安装器退出状态未确认，可能仍在写入；恢复记录已保留。确认安装器停止后运行 instance recover-loader {id} --installer-stopped；拒绝自动覆盖");
        }
        let profile = directory.join(format!("{id}.json"));
        let current = read(&profile, MAX_PROFILE)?;
        let mut rejected = None;
        if current != original {
            if let Some(bytes) = current {
                let output = directory.join(REJECTED);
                match fs::symlink_metadata(&output) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Ok(_) => bail!("加载器失败输出目录不是普通目录"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        fs::create_dir(&output)?
                    }
                    Err(error) => return Err(error).context("读取加载器失败输出目录失败"),
                }
                let path = output.join(format!("{:x}.json", Sha1::digest(&bytes)));
                match read(&path, MAX_PROFILE)? {
                    Some(previous) if previous != bytes => {
                        bail!("已有同名失败输出内容不匹配，拒绝覆盖")
                    }
                    Some(_) => {}
                    None => atomic_write(&path, &bytes)?,
                }
                rejected = Some(path);
            }
            match original {
                Some(bytes) => atomic_write(&profile, &bytes)?,
                None => match fs::remove_file(&profile) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("清除未完成加载器配置失败"),
                },
            }
        }
        fs::remove_file(directory.join(JOURNAL)).context("原配置已恢复，但无法清理恢复记录")?;
        Ok(Some(match rejected {
            Some(path) => format!("已恢复安装前的实例配置；失败输出保留于 {}", path.display()),
            None => "已恢复安装前的实例配置".into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance_service::InstanceLaunchSession;
    use serde_json::Value;
    fn profile(root: &Path, id: &str) -> PathBuf {
        let directory = root.join("versions").join(id);
        fs::create_dir_all(&directory).unwrap();
        directory.join(format!("{id}.json"))
    }

    #[test]
    fn failure_restores_original_bytes_preserves_output_and_success_keeps_new_profile() {
        let temp = crate::test_support::Workspace::new();
        let path = profile(&temp.path, "loader");
        fs::write(&path, b"original bytes").unwrap();
        let mutation = InstanceMutation::begin(&temp.path, "loader").unwrap();
        let mut transaction =
            LoaderProfileTransaction::begin(&temp.path, "loader", &mutation).unwrap();
        fs::write(&path, b"failed output").unwrap();
        transaction.rollback().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"original bytes");
        assert_eq!(
            fs::read_dir(path.parent().unwrap().join(REJECTED))
                .unwrap()
                .count(),
            1
        );
        transaction = LoaderProfileTransaction::begin(&temp.path, "loader", &mutation).unwrap();
        fs::write(&path, b"new verified profile").unwrap();
        transaction.commit().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new verified profile");
        assert!(!LoaderProfileTransaction::pending(&temp.path, "loader").unwrap());
    }

    #[test]
    fn failed_first_install_removes_only_profile_and_excludes_recovery_files_from_copy() {
        let temp = crate::test_support::Workspace::new();
        let mutation = InstanceMutation::begin(&temp.path, "loader").unwrap();
        let transaction = LoaderProfileTransaction::begin(&temp.path, "loader", &mutation).unwrap();
        let path = profile(&temp.path, "loader");
        fs::write(&path, b"failed").unwrap();
        fs::write(
            path.parent().unwrap().join("options.txt"),
            b"player settings",
        )
        .unwrap();
        transaction.rollback().unwrap();
        assert!(!path.exists());
        assert_eq!(
            fs::read(path.parent().unwrap().join("options.txt")).unwrap(),
            b"player settings"
        );
        let files = crate::instances::data_files(
            path.parent().unwrap(),
            true,
            &crate::task::TaskControl::default(),
        )
        .unwrap();
        assert!(files
            .iter()
            .all(|(_, name)| !name.contains(REJECTED) && !name.contains(JOURNAL)));
    }

    #[test]
    fn parent_configuration_is_recovered_before_launch_resolves_it() {
        let temp = crate::test_support::Workspace::new();
        let path = profile(&temp.path, "game");
        fs::write(&path, br#"{"id":"game","mainClass":"old.Main"}"#).unwrap();
        let mutation = InstanceMutation::begin(&temp.path, "game").unwrap();
        let transaction = LoaderProfileTransaction::begin(&temp.path, "game", &mutation).unwrap();
        fs::write(&path, b"broken").unwrap();
        drop(transaction);
        drop(mutation);
        fs::write(
            profile(&temp.path, "child"),
            br#"{"id":"child","inheritsFrom":"game"}"#,
        )
        .unwrap();
        let mut launch = InstanceLaunchSession::begin(&temp.path, "child").unwrap();
        let version = launch.resolve("child").unwrap();
        assert_eq!(version.meta["mainClass"], "old.Main");
        assert!(!launch.messages().is_empty());
    }

    #[test]
    fn unknown_installer_state_and_corrupt_backup_do_not_overwrite_current_files() {
        let temp = crate::test_support::Workspace::new();
        let path = profile(&temp.path, "loader");
        fs::write(&path, b"old").unwrap();
        let mutation = InstanceMutation::begin(&temp.path, "loader").unwrap();
        let mut transaction =
            LoaderProfileTransaction::begin(&temp.path, "loader", &mutation).unwrap();
        transaction.set_installer_active(true).unwrap();
        fs::write(&path, b"current").unwrap();
        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"current");
        transaction.set_installer_active(false).unwrap();
        let journal = path.parent().unwrap().join(JOURNAL);
        let mut record: Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
        record["original"] = serde_json::json!(STANDARD.encode(b"tampered"));
        fs::write(&journal, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"current");
    }

    #[test]
    fn explicit_recovery_cannot_bypass_a_live_shared_installer() {
        let temp = crate::test_support::Workspace::new();
        let path = profile(&temp.path, "loader");
        fs::write(&path, b"old").unwrap();
        let mutation = InstanceMutation::begin(&temp.path, "loader").unwrap();
        let mut transaction =
            LoaderProfileTransaction::begin(&temp.path, "loader", &mutation).unwrap();
        transaction.set_installer_active(true).unwrap();
        fs::write(&path, b"unfinished").unwrap();
        let journal = path.parent().unwrap().join(JOURNAL);
        let saved = fs::read(&journal).unwrap();
        drop(transaction);
        drop(mutation);
        let control = crate::task::TaskControl::default();
        let installer = crate::installation_service::LoaderInstallSession::acquire(
            &temp.path,
            &control,
            || panic!("unexpected installer contention"),
        )
        .unwrap();
        let error = LoaderProfileTransaction::recover_stopped(&temp.path, "loader").unwrap_err();
        assert!(format!("{error:#}").contains("安装任务占用"));
        assert_eq!(fs::read(&path).unwrap(), b"unfinished");
        assert_eq!(fs::read(&journal).unwrap(), saved);
        assert!(!path.parent().unwrap().join(REJECTED).exists());
        // Recovery does not parse, repair or overwrite shared launcher metadata.
        fs::write(
            temp.path.join("launcher_profiles.json"),
            b"unrelated damaged file",
        )
        .unwrap();
        drop(installer);
        LoaderProfileTransaction::recover_stopped(&temp.path, "loader").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(!journal.exists());
        assert_eq!(
            fs::read(temp.path.join("launcher_profiles.json")).unwrap(),
            b"unrelated damaged file"
        );
    }

    #[test]
    fn terminated_launcher_recovers_after_known_installer_exit_but_not_unknown_writes() {
        let temp = crate::test_support::Workspace::new();
        for active in [false, true] {
            let id = if active { "active" } else { "stopped" };
            let path = profile(&temp.path, id);
            fs::write(&path, b"old").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "loader_profile_service::tests::crash_helper",
                    "--exact",
                    "--ignored",
                ])
                .env("TUI_MC_LOADER_CRASH_ROOT", &temp.path)
                .env("TUI_MC_LOADER_CRASH_ID", id)
                .env("TUI_MC_LOADER_CRASH_ACTIVE", active.to_string())
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(73));
            let recovered = InstanceMutation::begin(&temp.path, id);
            if active {
                assert!(recovered.is_err());
                assert_eq!(fs::read(&path).unwrap(), b"interrupted");
                LoaderProfileTransaction::recover_stopped(&temp.path, id).unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"old");
            } else {
                recovered.unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"old");
            }
        }
    }

    #[test]
    #[ignore = "only invoked by the parent crash recovery test"]
    fn crash_helper() {
        let root = PathBuf::from(std::env::var_os("TUI_MC_LOADER_CRASH_ROOT").unwrap());
        let id = std::env::var("TUI_MC_LOADER_CRASH_ID").unwrap();
        let mutation = InstanceMutation::begin(&root, &id).unwrap();
        let mut transaction = LoaderProfileTransaction::begin(&root, &id, &mutation).unwrap();
        transaction
            .set_installer_active(std::env::var("TUI_MC_LOADER_CRASH_ACTIVE").unwrap() == "true")
            .unwrap();
        fs::write(profile(&root, &id), b"interrupted").unwrap();
        std::process::exit(73);
    }
}
