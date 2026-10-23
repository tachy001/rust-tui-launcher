use anyhow::{bail, Context, Result};
use std::{fs, path::Path};

/// Hold the existing world lock without modifying its contents or creating a
/// lock in old worlds. Unix uses the record lock used by Java NIO.
pub(crate) struct WorldSessionLock {
    file: Option<fs::File>,
    #[cfg(unix)]
    _local: LocalReservation,
}
impl Drop for WorldSessionLock {
    fn drop(&mut self) {
        // POSIX locks belong to the process. Close before releasing the local
        // reservation, so another thread cannot open/close this inode too soon.
        drop(self.file.take());
    }
}
impl WorldSessionLock {
    pub(crate) fn existing(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("无法检查世界占用锁"),
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                bail!("世界占用锁不能为链接或重解析文件");
            }
        }
        if !metadata.file_type().is_file() {
            bail!("世界占用锁不是普通文件");
        }
        #[cfg(unix)]
        let local = LocalReservation::acquire(&metadata)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .context("无法打开世界占用锁，请检查权限并退出该世界")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file.metadata()?;
            if (opened.dev(), opened.ino()) != (metadata.dev(), metadata.ino()) {
                bail!("世界占用锁在打开期间被替换，请重试");
            }
        }
        #[cfg(not(unix))]
        file.try_lock().context("世界正在使用，请先退出该世界")?;
        #[cfg(unix)]
        record_lock(&file)
            .context("世界正在使用或文件系统不支持 Java 兼容记录锁，请先退出该世界")?;
        Ok(Some(Self {
            file: Some(file),
            #[cfg(unix)]
            _local: local,
        }))
    }
}

#[cfg(unix)]
fn record_lock(file: &fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // libc supplies the target-specific struct layout and constants for macOS
    // and Linux. Zero length covers EOF and future growth, as OpenJDK does for
    // FileChannel.tryLock(0, Long.MAX_VALUE, false).
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 0;
    lock.l_len = 0;
    loop {
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
#[cfg(unix)]
type Identity = (u64, u64);
#[cfg(unix)]
fn reservations() -> &'static std::sync::Mutex<std::collections::HashSet<Identity>> {
    static HELD: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Identity>>> =
        std::sync::OnceLock::new();
    HELD.get_or_init(Default::default)
}
#[cfg(unix)]
struct LocalReservation(Identity);
#[cfg(unix)]
impl LocalReservation {
    fn acquire(metadata: &fs::Metadata) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let identity = (metadata.dev(), metadata.ino());
        let mut held = reservations()
            .lock()
            .map_err(|_| anyhow::anyhow!("世界锁状态不可用"))?;
        if !held.insert(identity) {
            bail!("该世界已被本进程中的其他操作占用");
        }
        Ok(Self(identity))
    }
}
#[cfg(unix)]
impl Drop for LocalReservation {
    fn drop(&mut self) {
        if let Ok(mut held) = reservations().lock() {
            held.remove(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_lock_is_not_created_and_existing_lock_is_preserved() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("session.lock");
        assert!(WorldSessionLock::existing(&path).unwrap().is_none());
        assert!(!path.exists());
        fs::write(&path, b"existing lock content").unwrap();
        let lock = WorldSessionLock::existing(&path).unwrap().unwrap();
        assert!(WorldSessionLock::existing(&path).is_err());
        drop(lock);
        WorldSessionLock::existing(&path).unwrap().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"existing lock content");
    }
    fn probe(path: &Path, expect_blocked: bool) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "world_lock_service::tests::child_record_lock_probe",
                "--nocapture",
            ])
            .env("TUI_WORLD_LOCK_PATH", path)
            .env(
                "TUI_WORLD_LOCK_BLOCKED",
                if expect_blocked { "yes" } else { "no" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("world-lock-probe-ok"));
    }
    #[test]
    fn child_record_lock_conflicts_until_guard_is_closed() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("session.lock");
        fs::write(&path, b"lock").unwrap();
        let lock = WorldSessionLock::existing(&path).unwrap().unwrap();
        assert!(WorldSessionLock::existing(&path).is_err());
        probe(&path, true);
        drop(lock);
        probe(&path, false);
    }
    #[test]
    #[ignore = "subprocess helper; uses POSIX record locks on Unix, never launches Java"]
    fn child_record_lock_probe() {
        let Some(path) = std::env::var_os("TUI_WORLD_LOCK_PATH") else {
            return;
        };
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        #[cfg(unix)]
        let blocked = record_lock(&file).is_err();
        #[cfg(not(unix))]
        let blocked = file.try_lock().is_err();
        assert_eq!(
            blocked,
            std::env::var("TUI_WORLD_LOCK_BLOCKED").unwrap() == "yes"
        );
        println!("world-lock-probe-ok");
    }
}
