use crate::task::TaskControl;
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

pub(crate) const LOCK_DIR: &str = ".tui-mc-download-locks";

/// An OS-backed lease, kept across retries and released on success, error or panic.
pub(crate) struct DownloadLease {
    _file: fs::File,
}
impl DownloadLease {
    fn open(destination: &Path) -> Result<fs::File> {
        let path = Self::lock_path(destination)?;
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            bail!("下载锁文件不能为符号链接");
        }
        Ok(fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?)
    }
    pub(crate) fn try_acquire(destination: &Path) -> Result<Self> {
        let file = Self::open(destination)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs::TryLockError::WouldBlock) => {
                bail!("共享游戏目录正在使用，请等待游戏或其他修改任务结束")
            }
            Err(fs::TryLockError::Error(error)) => Err(error).context("无法获取共享目录锁"),
        }
    }
    fn lock_path(destination: &Path) -> Result<PathBuf> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let parent = fs::canonicalize(parent)?;
        let directory = parent.join(LOCK_DIR);
        fs::create_dir_all(&directory)?;
        if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
            bail!("下载锁目录不能为符号链接");
        }
        let name = destination
            .file_name()
            .context("下载目标没有文件名")?
            .to_string_lossy();
        let name = if cfg!(any(windows, target_os = "macos")) {
            name.to_lowercase()
        } else {
            name.into_owned()
        };
        let key = format!("{:x}.lock", Sha1::digest(name.as_bytes()));
        Ok(directory.join(key))
    }
    pub(crate) fn acquire(
        destination: &Path,
        control: &TaskControl,
        mut on_wait: impl FnMut(),
    ) -> Result<Self> {
        control.check()?;
        let file = Self::open(destination)?;
        let mut reported = false;
        loop {
            control.check()?;
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if !reported {
                        on_wait();
                        reported = true;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(error).context("无法获取共享下载锁")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    #[test]
    fn wait_can_be_cancelled_without_releasing_the_other_tasks_lease() {
        let workspace = crate::test_support::Workspace::new();
        let dest = workspace.path.join("file.bin");
        let held = DownloadLease::acquire(&dest, &TaskControl::default(), || {}).unwrap();
        let control = TaskControl::default();
        let worker_control = control.clone();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            DownloadLease::acquire(&dest, &worker_control, || {
                tx.send(()).unwrap();
            })
        });
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        control.cancel();
        assert!(handle.join().unwrap().is_err());
        let other = workspace.path.join("other.bin");
        let _independent = DownloadLease::acquire(&other, &TaskControl::default(), || {}).unwrap();
        drop(held);
        let _available = DownloadLease::acquire(
            &workspace.path.join("file.bin"),
            &TaskControl::default(),
            || {},
        )
        .unwrap();
    }
    #[test]
    fn lock_metadata_is_excluded_from_export_and_cannot_be_imported() {
        let workspace = crate::test_support::Workspace::new();
        let dest = workspace.path.join("mods/example.jar");
        let _lease = DownloadLease::acquire(&dest, &TaskControl::default(), || {}).unwrap();
        fs::write(&dest, b"mod").unwrap();
        let files =
            crate::instances::data_files(&workspace.path, false, &TaskControl::default()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, "mods/example.jar");
        assert!(crate::pack::file_destination(
            &workspace.path,
            &format!("mods/{LOCK_DIR}/entry.lock"),
            "game"
        )
        .is_err());
    }
    #[test]
    fn another_process_waits_for_the_same_destination() {
        let workspace = crate::test_support::Workspace::new();
        let dest = workspace.path.join("file.bin");
        let held = DownloadLease::acquire(&dest, &TaskControl::default(), || {}).unwrap();
        let ready = workspace.path.join("waiting");
        let acquired = workspace.path.join("acquired");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "download_lock::tests::child_process_probe",
            ])
            .env("TUI_DOWNLOAD_LEASE_DEST", &dest)
            .env("TUI_DOWNLOAD_LEASE_READY", &ready)
            .env("TUI_DOWNLOAD_LEASE_ACQUIRED", &acquired)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut child = crate::backend::GameProcess::from_child(child);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(child.try_wait().unwrap().is_none(), "子进程意外退出");
            assert!(std::time::Instant::now() < deadline, "子进程未进入等待");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!acquired.exists());
        drop(held);
        while child.try_wait().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "子进程未释放等待");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(child.wait().unwrap().success());
        assert!(acquired.exists());
    }
    #[test]
    #[ignore = "subprocess helper for cross-process lease verification"]
    fn child_process_probe() {
        let Some(dest) = std::env::var_os("TUI_DOWNLOAD_LEASE_DEST") else {
            return;
        };
        let ready = std::env::var_os("TUI_DOWNLOAD_LEASE_READY").unwrap();
        let acquired = std::env::var_os("TUI_DOWNLOAD_LEASE_ACQUIRED").unwrap();
        let _lease = DownloadLease::acquire(Path::new(&dest), &TaskControl::default(), || {
            fs::write(&ready, b"waiting").unwrap();
        })
        .unwrap();
        fs::write(acquired, b"acquired").unwrap();
    }
}
