use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static SERIAL: AtomicU64 = AtomicU64::new(0);
/// Owns only a create_new-confirmed temporary file, until rename publishes it.
struct AtomicFile {
    destination: PathBuf,
    temporary: PathBuf,
    file: Option<fs::File>,
    committed: bool,
}
impl AtomicFile {
    fn create(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        path.file_name().context("文件路径没有名称")?;
        let temporary = parent.join(format!(
            ".tui-mc-write-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        Self::open(path, temporary)
    }
    fn open(destination: &Path, temporary: PathBuf) -> Result<Self> {
        if destination.parent() == temporary.parent()
            && destination
                .file_name()
                .zip(temporary.file_name())
                .is_some_and(|(a, b)| {
                    a.to_string_lossy()
                        .eq_ignore_ascii_case(&b.to_string_lossy())
                })
        {
            bail!("临时文件不能与最终目标同名");
        }
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        // Never construct a cleanup owner if create_new failed.
        Ok(Self {
            destination: destination.into(),
            temporary,
            file: Some(file),
            committed: false,
        })
    }
    fn writer(&mut self) -> &mut fs::File {
        self.file.as_mut().expect("uncommitted atomic file")
    }
    fn commit(mut self, control: Option<&crate::task::TaskControl>) -> Result<()> {
        if let Some(control) = control {
            control.check()?;
        }
        let file = self.file.take().expect("uncommitted atomic file");
        file.sync_all()?;
        drop(file);
        if let Some(control) = control {
            control.check()?;
        }
        fs::rename(&self.temporary, &self.destination).context("无法替换目标文件；原文件已保留")?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for AtomicFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.committed {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = AtomicFile::create(path)?;
    file.writer().write_all(bytes)?;
    file.commit(None)
}
pub fn atomic_copy(path: &Path, reader: &mut impl Read) -> Result<u64> {
    let mut file = AtomicFile::create(path)?;
    let count = std::io::copy(reader, file.writer())?;
    file.commit(None)?;
    Ok(count)
}
pub(crate) fn atomic_copy_controlled(
    path: &Path,
    reader: &mut impl Read,
    control: &crate::task::TaskControl,
) -> Result<u64> {
    control.check()?;
    let mut file = AtomicFile::create(path)?;
    let mut buffer = [0; 65536];
    let mut count = 0_u64;
    loop {
        control.check()?;
        let size = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        control.check()?;
        if size == 0 {
            break;
        }
        file.writer().write_all(&buffer[..size])?;
        count = count.checked_add(size as u64).context("复制大小溢出")?;
    }
    file.commit(Some(control))?;
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskControl;
    use std::io::Cursor;

    fn no_temporary(root: &Path) {
        assert!(fs::read_dir(root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }
    #[test]
    fn cancelled_during_read_or_at_eof_preserves_original_and_cleans_owned_stage() {
        struct CancelReader {
            control: TaskControl,
            at_eof: bool,
            read: bool,
        }
        impl Read for CancelReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.read {
                    self.control.cancel();
                    return Ok(0);
                }
                self.read = true;
                if !self.at_eof {
                    self.control.cancel();
                }
                buffer[..3].copy_from_slice(b"new");
                Ok(3)
            }
        }
        let temp = crate::test_support::Workspace::new();
        let dest = temp.path.join("target.bin");
        fs::write(&dest, b"old").unwrap();
        for at_eof in [false, true] {
            let control = TaskControl::default();
            let mut reader = CancelReader {
                control: control.clone(),
                at_eof,
                read: false,
            };
            assert!(atomic_copy_controlled(&dest, &mut reader, &control).is_err());
            assert_eq!(fs::read(&dest).unwrap(), b"old");
            no_temporary(&temp.path);
        }
        let control = TaskControl::default();
        control.cancel();
        assert!(atomic_copy_controlled(
            &temp.path.join("missing/target"),
            &mut Cursor::new(b"new"),
            &control
        )
        .is_err());
        assert!(!temp.path.join("missing").exists());
    }

    #[test]
    fn interrupted_reads_retry_and_copy_errors_never_publish_partial_data() {
        struct Reader {
            step: u8,
            failure: bool,
        }
        impl Read for Reader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.step += 1;
                match self.step {
                    1 => Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                    2 => {
                        buffer[..3].copy_from_slice(b"abc");
                        Ok(3)
                    }
                    _ if self.failure => Err(std::io::Error::other("simulated source failure")),
                    _ => Ok(0),
                }
            }
        }
        let temp = crate::test_support::Workspace::new();
        let dest = temp.path.join("target.bin");
        fs::write(&dest, b"old").unwrap();
        assert!(atomic_copy_controlled(
            &dest,
            &mut Reader {
                step: 0,
                failure: true
            },
            &TaskControl::default()
        )
        .is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"old");
        assert_eq!(
            atomic_copy_controlled(
                &dest,
                &mut Reader {
                    step: 0,
                    failure: false
                },
                &TaskControl::default()
            )
            .unwrap(),
            3
        );
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
        no_temporary(&temp.path);
    }

    #[test]
    fn failed_creation_never_removes_foreign_file_and_failed_publish_keeps_target() {
        let temp = crate::test_support::Workspace::new();
        let foreign = temp.path.join("foreign.tmp");
        let dest = temp.path.join("target");
        assert!(AtomicFile::open(&dest, dest.clone()).is_err());
        assert!(!dest.exists());
        fs::write(&foreign, b"not owned by this attempt").unwrap();
        assert!(AtomicFile::open(&dest, foreign.clone()).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"not owned by this attempt");
        fs::remove_file(foreign).unwrap();
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("keep"), b"original directory").unwrap();
        assert!(
            atomic_copy_controlled(&dest, &mut Cursor::new(b"new"), &TaskControl::default())
                .is_err()
        );
        assert_eq!(fs::read(dest.join("keep")).unwrap(), b"original directory");
        no_temporary(&temp.path);
    }
    #[test]
    fn replaces_existing_file() {
        let path = std::env::temp_dir().join(format!("tui-mc-atomic-{}", std::process::id()));
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_file(path).unwrap();
    }
}
