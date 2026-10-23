use crate::{
    download_lock::{DownloadLease, LOCK_DIR},
    metadata::safe_path,
    storage::atomic_write,
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const RUNTIME_SESSION: &str = ".tui-mc-runtime-install";
const INSTALLER_SESSION: &str = ".tui-mc-loader-install";

/// Serializes an entire component installation, including markers and executable validation.
pub(crate) struct RuntimeInstallSession<'a> {
    root: PathBuf,
    fingerprint: String,
    control: &'a TaskControl,
    _lease: DownloadLease,
}
impl<'a> RuntimeInstallSession<'a> {
    pub(crate) fn begin(
        root: &Path,
        fingerprint: &str,
        control: &'a TaskControl,
        on_wait: impl FnMut(),
    ) -> Result<Self> {
        if fingerprint.len() != 40 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("Java 运行时清单缺少有效 SHA-1，拒绝写入安装状态");
        }
        let lease = DownloadLease::acquire(&root.join(RUNTIME_SESSION), control, on_wait)?;
        control.check()?;
        atomic_write(
            &root.join(".installing"),
            b"Java runtime installation in progress",
        )?;
        Ok(Self {
            root: root.into(),
            fingerprint: fingerprint.into(),
            control,
            _lease: lease,
        })
    }
    pub(crate) fn destination(&self, name: &str) -> Result<PathBuf> {
        let path = safe_path(&self.root, name)?;
        for component in path.strip_prefix(&self.root)?.components() {
            if let Component::Normal(name) = component {
                let name = name.to_string_lossy().to_ascii_lowercase();
                if [
                    ".installing",
                    ".complete",
                    ".manifest.json",
                    RUNTIME_SESSION,
                    LOCK_DIR,
                ]
                .contains(&name.as_str())
                {
                    bail!("Java 文件清单不能覆盖启动器保留路径：{name}");
                }
            }
        }
        Ok(path)
    }
    pub(crate) fn finish(self) -> Result<()> {
        self.control.check()?;
        atomic_write(&self.root.join(".complete"), self.fingerprint.as_bytes())?;
        fs::remove_file(self.root.join(".installing"))
            .context("Java 安装完成，但无法清除安装中标记")?;
        Ok(())
    }
}

/// Different loader versions still share launcher_profiles.json and installer workspace.
pub(crate) struct LoaderInstallSession {
    _lease: DownloadLease,
}
impl LoaderInstallSession {
    /// Recovery must exclude live installers without creating / editing their
    /// launcher_profiles.json. Fail fast so an explicit assertion cannot bypass
    /// an installer that this launcher can authoritatively observe as active.
    pub(crate) fn for_recovery(root: &Path) -> Result<Self> {
        let lease = DownloadLease::try_acquire(&root.join(INSTALLER_SESSION))
            .context("游戏目录仍有安装任务占用，暂不能恢复加载器配置")?;
        Ok(Self { _lease: lease })
    }

    pub(crate) fn acquire(
        root: &Path,
        control: &TaskControl,
        on_wait: impl FnMut(),
    ) -> Result<Self> {
        let lease = DownloadLease::acquire(&root.join(INSTALLER_SESSION), control, on_wait)?;
        control.check()?;
        let path = root.join("launcher_profiles.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)
                    .context("launcher_profiles.json 格式损坏，请修正后重试；原文件已保留")?;
                if !value.is_object()
                    || value
                        .get("profiles")
                        .is_some_and(|profiles| !profiles.is_object())
                {
                    bail!(
                        "launcher_profiles.json 必须为包含有效 profiles 对象的配置，原文件已保留"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                control.check()?;
                atomic_write(&path, br#"{"profiles":{},"selectedProfile":""}"#)?;
            }
            Err(error) => {
                return Err(error).context("读取 launcher_profiles.json 失败，未覆盖原文件")
            }
        }
        Ok(Self { _lease: lease })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};
    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
    #[test]
    fn completion_is_published_only_after_the_owning_session_finishes() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let session = RuntimeInstallSession::begin(&workspace.path, HASH, &control, || {}).unwrap();
        assert!(workspace.path.join(".installing").is_file());
        assert!(!workspace.path.join(".complete").exists());
        session.finish().unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path.join(".complete")).unwrap(),
            HASH
        );
        assert!(!workspace.path.join(".installing").exists());
    }
    #[test]
    fn cancelled_or_failed_sessions_remain_incomplete_and_cannot_publish_success() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let session = RuntimeInstallSession::begin(&workspace.path, HASH, &control, || {}).unwrap();
        control.cancel();
        assert!(session.finish().is_err());
        assert!(workspace.path.join(".installing").is_file());
        assert!(!workspace.path.join(".complete").exists());
        let invalid = workspace.path.join("invalid");
        assert!(RuntimeInstallSession::begin(
            &invalid,
            "not-a-hash",
            &TaskControl::default(),
            || {}
        )
        .is_err());
        assert!(!invalid.exists());
    }
    #[test]
    fn cancelling_a_waiter_does_not_change_the_active_installers_marker() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let first = RuntimeInstallSession::begin(&workspace.path, HASH, &control, || {}).unwrap();
        fs::write(workspace.path.join(".installing"), b"first owner").unwrap();
        let next_control = TaskControl::default();
        let worker_control = next_control.clone();
        let root = workspace.path.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            RuntimeInstallSession::begin(&root, HASH, &worker_control, || {
                tx.send(()).unwrap();
            })
            .is_err()
        });
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        next_control.cancel();
        assert!(worker.join().unwrap());
        assert_eq!(
            fs::read(workspace.path.join(".installing")).unwrap(),
            b"first owner"
        );
        first.finish().unwrap();
    }
    #[test]
    fn independent_runtime_components_can_install_and_reserved_paths_are_rejected() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let first =
            RuntimeInstallSession::begin(&workspace.path.join("java17"), HASH, &control, || {})
                .unwrap();
        let second =
            RuntimeInstallSession::begin(&workspace.path.join("java21"), HASH, &control, || {})
                .unwrap();
        for name in [
            ".complete",
            ".INSTALLING",
            "../escape",
            ".manifest.json",
            "lib/.tui-mc-download-locks/overwrite",
        ] {
            assert!(first.destination(name).is_err());
        }
        assert!(first.destination("bin/java").unwrap().ends_with("bin/java"));
        first.finish().unwrap();
        second.finish().unwrap();
    }
    #[test]
    fn loader_session_preserves_existing_profiles_and_initializes_only_missing_files() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let bytes = br#"{"profiles":{"custom":{"name":"Keep"}},"selectedProfile":"custom"}"#;
        fs::write(workspace.path.join("launcher_profiles.json"), bytes).unwrap();
        let _session = LoaderInstallSession::acquire(&workspace.path, &control, || {}).unwrap();
        assert_eq!(
            fs::read(workspace.path.join("launcher_profiles.json")).unwrap(),
            bytes
        );
        let other = workspace.path.join("other");
        let _other = LoaderInstallSession::acquire(&other, &control, || {}).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(other.join("launcher_profiles.json")).unwrap())
                .unwrap();
        assert!(value["profiles"].is_object());
    }
    #[test]
    fn loader_installers_share_one_root_lease_until_the_previous_owner_exits() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let first = LoaderInstallSession::acquire(&workspace.path, &control, || {}).unwrap();
        let root = workspace.path.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            LoaderInstallSession::acquire(&root, &TaskControl::default(), || {
                tx.send(()).unwrap();
            })
            .is_ok()
        });
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!worker.is_finished());
        drop(first);
        assert!(worker.join().unwrap());
    }
    #[test]
    fn malformed_or_unreadable_profiles_are_never_overwritten() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("launcher_profiles.json");
        fs::write(&path, b"{broken").unwrap();
        assert!(
            LoaderInstallSession::acquire(&workspace.path, &TaskControl::default(), || {}).is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(
            LoaderInstallSession::acquire(&workspace.path, &TaskControl::default(), || {}).is_err()
        );
        assert!(path.is_dir());
    }
}
