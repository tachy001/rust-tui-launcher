use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Publish a complete file or directory in one filesystem move. The kernel
/// checks destination absence; never emulate this with exists() then rename().
pub(crate) struct NoReplacePublisher;
impl NoReplacePublisher {
    pub(crate) fn publish(&self, source: &Path, destination: &Path) -> io::Result<()> {
        fn absolute_leaf(path: &Path) -> io::Result<PathBuf> {
            let name = path
                .file_name()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "发布路径缺少文件名"))?;
            let parent = path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(fs::canonicalize(parent)?.join(name))
        }
        let source = absolute_leaf(source)?;
        let destination = absolute_leaf(destination)?;
        let same = if cfg!(windows) {
            source
                .as_os_str()
                .eq_ignore_ascii_case(destination.as_os_str())
        } else {
            source == destination
        };
        if same {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "发布源与目标不能相同",
            ));
        }
        native_publish(&source, &destination)
    }
}

#[cfg(windows)]
fn native_publish(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut value: Vec<_> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "发布路径含 NUL",
            ));
        }
        value.push(0);
        Ok(value)
    }
    let source = wide(source)?;
    let destination = wide(destination)?;
    // No REPLACE_EXISTING and no COPY_ALLOWED: never overwrite or copy a
    // partially visible destination on a different volume.
    let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0x8) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_publish(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    let result = {
        unsafe extern "C" {
            fn renameat2(
                oldfd: i32,
                old: *const std::ffi::c_char,
                newfd: i32,
                new: *const std::ffi::c_char,
                flags: u32,
            ) -> i32;
        }
        // AT_FDCWD; RENAME_NOREPLACE. Unsupported kernels/filesystems fail
        // without falling back to an overwriting rename.
        unsafe { renameat2(-100, source.as_ptr(), -100, destination.as_ptr(), 1) }
    };
    #[cfg(target_os = "macos")]
    let result = {
        unsafe extern "C" {
            fn renamex_np(
                old: *const std::ffi::c_char,
                new: *const std::ffi::c_char,
                flags: u32,
            ) -> i32;
        }
        // RENAME_EXCL, available from macOS 10.12.
        unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), 4) }
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn native_publish(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "此平台尚不支持无覆盖发布",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_commit_preserves_existing_destination_and_supports_unicode_paths() {
        let temp = crate::test_support::Workspace::new();
        let source = temp.path.join("暂存 file");
        let destination = temp.path.join("输出 file.zip");
        fs::write(&source, b"complete new data").unwrap();
        assert_eq!(
            NoReplacePublisher
                .publish(&source, &source)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        fs::write(&destination, b"existing data").unwrap();
        assert!(NoReplacePublisher.publish(&source, &destination).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"complete new data");
        assert_eq!(fs::read(&destination).unwrap(), b"existing data");
        fs::remove_file(&destination).unwrap();
        NoReplacePublisher.publish(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"complete new data");
    }
    #[test]
    fn existing_empty_directory_is_not_replaced_by_an_import() {
        let temp = crate::test_support::Workspace::new();
        let source = temp.path.join("staging");
        let destination = temp.path.join("world");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("level.dat"), b"world").unwrap();
        fs::create_dir(&destination).unwrap();
        assert!(NoReplacePublisher.publish(&source, &destination).is_err());
        assert!(source.join("level.dat").is_file());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
        fs::remove_dir(&destination).unwrap();
        NoReplacePublisher.publish(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(destination.join("level.dat")).unwrap(), b"world");
    }
    #[test]
    fn concurrent_publishers_have_exactly_one_winner_and_keep_losing_source() {
        let temp = crate::test_support::Workspace::new();
        let destination = temp.path.join("result");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = vec![];
        for index in 0..2 {
            let source = temp.path.join(format!("source-{index}"));
            fs::write(&source, [index as u8]).unwrap();
            let destination = destination.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let success = NoReplacePublisher.publish(&source, &destination).is_ok();
                (index, source, success)
            }));
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|(_, _, success)| *success).count(), 1);
        for (index, source, success) in results {
            if success {
                assert_eq!(fs::read(&destination).unwrap(), [index as u8]);
                assert!(!source.exists());
            } else {
                assert_eq!(fs::read(source).unwrap(), [index as u8]);
            }
        }
    }
}
