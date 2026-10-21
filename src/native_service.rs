use crate::{metadata::safe_path, task::TaskControl};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static SERIAL: AtomicU64 = AtomicU64::new(0);

/// A launch owns fresh native files until its child process has been reaped.
pub(crate) struct NativeDirectory<'a> {
    canonical: PathBuf,
    parent: PathBuf,
    java_path: PathBuf,
    control: &'a TaskControl,
    failed: bool,
}
impl<'a> NativeDirectory<'a> {
    pub(crate) fn create(parent: &Path, control: &'a TaskControl) -> Result<Self> {
        control.check()?;
        fs::create_dir_all(parent)?;
        let parent = fs::canonicalize(parent)?;
        let directory = parent.join(format!(
            "run-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).context("创建本次启动的 native 目录失败")?;
        Ok(Self {
            java_path: crate::paths::for_java(&directory),
            canonical: directory,
            parent,
            control,
            failed: false,
        })
    }
    pub(crate) fn path(&self) -> Result<&Path> {
        if self.failed {
            bail!("native 解压未完成，不能使用部分文件启动游戏");
        }
        self.control.check()?;
        Ok(&self.java_path)
    }
    pub(crate) fn extract(&mut self, archive: &Path, library: &Value) -> Result<()> {
        if self.failed {
            bail!("native 目录已有解压失败，不能继续使用");
        }
        let result = self
            .extract_inner(archive, library)
            .with_context(|| format!("解压 native 库失败：{}", archive.display()));
        self.failed |= result.is_err();
        result
    }
    fn extract_inner(&self, path: &Path, library: &Value) -> Result<()> {
        self.control.check()?;
        let excludes = match library.pointer("/extract/exclude") {
            None => vec![],
            Some(value) => value
                .as_array()
                .context("native extract.exclude 必须为数组")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(|value| value.replace('\\', "/"))
                        .context("native 排除项必须为字符串")
                })
                .collect::<Result<Vec<_>>>()?,
        };
        let mut archive = zip::ZipArchive::new(fs::File::open(path)?)?;
        let mut buffer = [0; 65536];
        for index in 0..archive.len() {
            self.control.check()?;
            let mut entry = archive.by_index(index)?;
            let name = entry.name().replace('\\', "/");
            if name.starts_with("META-INF/")
                || excludes.iter().any(|prefix| name.starts_with(prefix))
            {
                continue;
            }
            let dest = safe_path(&self.canonical, name.trim_end_matches('/'))?;
            if entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                bail!("native ZIP 不能包含符号链接：{name}");
            }
            if entry.is_dir() {
                continue;
            }
            fs::create_dir_all(dest.parent().context("native 文件缺少父目录")?)?;
            let mut file = fs::File::create(dest)?;
            loop {
                self.control.check()?;
                let size = entry.read(&mut buffer)?;
                if size == 0 {
                    break;
                }
                file.write_all(&buffer[..size])?;
            }
        }
        Ok(())
    }
}
impl Drop for NativeDirectory<'_> {
    fn drop(&mut self) {
        // Delete only the directory created by this guard, with both resolved paths rechecked.
        if fs::canonicalize(&self.parent).ok().as_ref() != Some(&self.parent) {
            return;
        }
        let Ok(resolved) = fs::canonicalize(&self.canonical) else {
            return;
        };
        if resolved == self.canonical
            && resolved.parent() == Some(self.parent.as_path())
            && resolved
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("run-"))
        {
            let _ = fs::remove_dir_all(resolved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(path: &Path, entries: &[(&str, &[u8])]) {
        let mut writer = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, bytes) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    #[test]
    fn fresh_launch_keeps_all_platform_libraries_without_reusing_old_files() {
        let workspace = crate::test_support::Workspace::new();
        let parent = workspace.path.join("natives");
        fs::create_dir_all(&parent).unwrap();
        fs::write(parent.join("old.dll"), b"old").unwrap();
        let zip = workspace.path.join("native.jar");
        archive(
            &zip,
            &[
                ("a.dll", b"win"),
                ("linux/liba.so", b"linux"),
                ("liba.dylib", b"mac"),
                ("META-INF/signature", b"skip"),
                ("excluded\\extra.dll", b"skip"),
            ],
        );
        let control = TaskControl::default();
        let mut native = NativeDirectory::create(&parent, &control).unwrap();
        let path = native.path().unwrap().to_path_buf();
        native
            .extract(
                &zip,
                &serde_json::json!({"extract":{"exclude":["excluded/"]}}),
            )
            .unwrap();
        assert!(!path.join("old.dll").exists());
        assert_eq!(fs::read(path.join("a.dll")).unwrap(), b"win");
        assert_eq!(fs::read(path.join("linux/liba.so")).unwrap(), b"linux");
        assert_eq!(fs::read(path.join("liba.dylib")).unwrap(), b"mac");
        assert!(!path.join("META-INF").exists());
        assert!(!path.join("excluded").exists());
        drop(native);
        assert!(!path.exists());
        assert_eq!(fs::read(parent.join("old.dll")).unwrap(), b"old");
    }
    #[test]
    fn partial_extraction_failure_is_unusable_and_only_its_own_files_are_removed() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let parent = workspace.path.join("natives");
        let mut native = NativeDirectory::create(&parent, &control).unwrap();
        let path = native.path().unwrap().to_path_buf();
        let other = NativeDirectory::create(&parent, &control).unwrap();
        let other_path = other.path().unwrap().to_path_buf();
        fs::write(other_path.join("running.so"), b"keep").unwrap();
        let zip = workspace.path.join("bad.jar");
        archive(
            &zip,
            &[("partial.dll", b"partial"), ("../../escape.dll", b"bad")],
        );
        assert!(native.extract(&zip, &serde_json::json!({})).is_err());
        assert!(native.path().is_err());
        assert!(path.join("partial.dll").exists());
        drop(native);
        assert!(!path.exists());
        assert_eq!(fs::read(other_path.join("running.so")).unwrap(), b"keep");
        assert!(!workspace.path.join("escape.dll").exists());
    }
    #[test]
    fn cancellation_prevents_extraction_and_cleans_the_owned_directory() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let mut native =
            NativeDirectory::create(&workspace.path.join("natives"), &control).unwrap();
        let path = native.path().unwrap().to_path_buf();
        control.cancel();
        assert!(native
            .extract(&workspace.path.join("missing.jar"), &serde_json::json!({}))
            .is_err());
        assert!(native.path().is_err());
        drop(native);
        assert!(!path.exists());
    }
    #[test]
    fn corrupt_archive_and_invalid_exclusion_metadata_are_not_silently_accepted() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let zip = workspace.path.join("bad.jar");
        fs::write(&zip, b"not a ZIP").unwrap();
        let mut native =
            NativeDirectory::create(&workspace.path.join("natives"), &control).unwrap();
        assert!(native.extract(&zip, &serde_json::json!({})).is_err());
        assert!(native.path().is_err());
        let mut other = NativeDirectory::create(&workspace.path.join("natives"), &control).unwrap();
        assert!(other
            .extract(&zip, &serde_json::json!({"extract":{"exclude":[42]}}))
            .is_err());
    }
    #[test]
    fn symlink_entries_are_rejected_without_creating_links_on_any_platform() {
        let workspace = crate::test_support::Workspace::new();
        let zip = workspace.path.join("link.jar");
        let mut writer = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
        writer
            .add_symlink(
                "lib.so",
                "outside",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.finish().unwrap();
        let control = TaskControl::default();
        let mut native =
            NativeDirectory::create(&workspace.path.join("natives"), &control).unwrap();
        assert!(native.extract(&zip, &serde_json::json!({})).is_err());
        assert!(native.path().is_err());
    }
}
