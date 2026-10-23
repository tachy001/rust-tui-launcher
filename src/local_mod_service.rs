use crate::{
    config::Config, download_lock::DownloadLease, instance_service::InstanceMutation,
    metadata::safe_path, task::TaskControl,
};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static SERIAL: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ModMutationSession {
    directory: PathBuf,
    _instance: InstanceMutation,
    _game: DownloadLease,
}

struct StagedMod(PathBuf);
impl Drop for StagedMod {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
impl ModMutationSession {
    pub(crate) fn lock_game_directory(config: &Config, id: &str) -> Result<DownloadLease> {
        let directory = config.effective(id)?.instance_dir(id)?;
        DownloadLease::try_acquire(&directory.join("options.txt"))
    }
    pub(crate) fn begin(config: &Config, id: &str) -> Result<Self> {
        let root = config.root()?;
        let instance = InstanceMutation::begin(&root, id)?;
        let game_dir = config.effective(id)?.instance_dir(id)?;
        let game = Self::lock_game_directory(config, id)?;
        let directory = game_dir.join("mods");
        fs::create_dir_all(&directory)?;
        if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
            bail!("Mod 目录不能为符号链接");
        }
        Ok(Self {
            directory,
            _instance: instance,
            _game: game,
        })
    }

    pub(crate) fn toggle(&self, filename: &str) -> Result<()> {
        crate::metadata::valid_id(filename)?;
        let source = safe_path(&self.directory, filename)?;
        let metadata = fs::symlink_metadata(&source)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("请选择普通 JAR Mod 文件");
        }
        let lower = filename.to_ascii_lowercase();
        let target = if lower.ends_with(".jar.disabled") {
            filename[..filename.len() - 9].to_owned()
        } else if lower.ends_with(".jar") {
            format!("{filename}.disabled")
        } else {
            bail!("请选择 JAR Mod 文件");
        };
        let target = safe_path(&self.directory, &target)?;
        // A hard link publishes atomically and fails if the destination exists.
        // Unlike rename, it cannot replace a concurrently created target on Unix.
        fs::hard_link(&source, &target)
            .context("无法切换 Mod；目标可能已存在，或文件系统不支持硬链接")?;
        if let Err(error) = fs::remove_file(&source) {
            if let Err(rollback) = fs::remove_file(&target) {
                bail!("切换 Mod 失败：{error}；撤回新名称失败：{rollback}，两份文件已保留");
            }
            return Err(error).context("切换 Mod 失败，原文件已保留");
        }
        Ok(())
    }

    pub(crate) fn import(&self, source: &Path, control: &TaskControl) -> Result<()> {
        let filename = source
            .file_name()
            .and_then(|name| name.to_str())
            .context("Mod 文件名不是有效 UTF-8")?;
        self.import_reader(filename, &mut fs::File::open(source)?, control)
    }

    fn import_reader(
        &self,
        filename: &str,
        reader: &mut dyn Read,
        control: &TaskControl,
    ) -> Result<()> {
        control.check()?;
        crate::metadata::valid_id(filename)?;
        if !filename.to_ascii_lowercase().ends_with(".jar") {
            bail!("只能导入 .jar 文件");
        }
        let target = safe_path(&self.directory, filename)?;
        let disabled = safe_path(&self.directory, &format!("{filename}.disabled"))?;
        if fs::symlink_metadata(&target).is_ok() || fs::symlink_metadata(&disabled).is_ok() {
            bail!("已有同名启用或禁用 Mod，不会覆盖或创建双副本");
        }
        let staged_path = self.directory.join(format!(
            ".tui-mc-import-{}-{}.part",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged_path)?;
        // Own cleanup only after create_new confirms this attempt created it.
        let staged = StagedMod(staged_path);
        let copied = (|| -> Result<()> {
            let mut buffer = [0; 65536];
            loop {
                control.check()?;
                let size = reader.read(&mut buffer)?;
                if size == 0 {
                    break;
                }
                output.write_all(&buffer[..size])?;
            }
            output.sync_all()?;
            Ok(())
        })();
        drop(output);
        copied?;
        control.check()?;
        zip::ZipArchive::new(fs::File::open(&staged.0)?)
            .context("文件不是有效的 JAR 归档，未发布到 Mod 目录")?;
        control.check()?;
        fs::hard_link(&staged.0, &target)
            .context("无法发布 Mod；目标可能已存在，或文件系统不支持硬链接")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn config(path: &Path) -> Config {
        Config {
            game_dir: path.into(),
            isolated: false,
            ..Config::default()
        }
    }
    fn jar() -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(vec![]));
        writer
            .start_file("mod.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"fixture").unwrap();
        writer.finish().unwrap().into_inner()
    }
    fn no_partial(directory: &Path) {
        assert!(fs::read_dir(directory).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".part")));
    }

    #[test]
    fn import_validates_staged_bytes_preserves_disabled_counterpart_and_toggles_without_overwrite()
    {
        let temp = crate::test_support::Workspace::new();
        let session = ModMutationSession::begin(&config(&temp.path), "test").unwrap();
        let bytes = jar();
        session
            .import_reader(
                "example.jar",
                &mut Cursor::new(&bytes),
                &TaskControl::default(),
            )
            .unwrap();
        session.toggle("example.jar").unwrap();
        assert_eq!(
            fs::read(session.directory.join("example.jar.disabled")).unwrap(),
            bytes
        );
        assert!(session
            .import_reader(
                "example.jar",
                &mut Cursor::new(jar()),
                &TaskControl::default()
            )
            .is_err());
        fs::write(session.directory.join("example.jar"), b"keep").unwrap();
        assert!(session.toggle("example.jar.disabled").is_err());
        assert_eq!(
            fs::read(session.directory.join("example.jar")).unwrap(),
            b"keep"
        );
        no_partial(&session.directory);
    }

    struct BrokenReader(bool);
    impl Read for BrokenReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other("simulated interrupted copy"));
            }
            self.0 = true;
            buffer[..3].copy_from_slice(b"bad");
            Ok(3)
        }
    }
    #[test]
    fn copy_failure_invalid_archive_and_cancellation_never_publish_partial_mods() {
        let temp = crate::test_support::Workspace::new();
        let session = ModMutationSession::begin(&config(&temp.path), "test").unwrap();
        assert!(session
            .import_reader(
                "broken.jar",
                &mut BrokenReader(false),
                &TaskControl::default()
            )
            .is_err());
        assert!(session
            .import_reader(
                "broken.jar",
                &mut Cursor::new(b"bad"),
                &TaskControl::default()
            )
            .is_err());
        let control = TaskControl::default();
        control.cancel();
        assert!(session
            .import_reader("cancelled.jar", &mut Cursor::new(jar()), &control)
            .is_err());
        assert!(!session.directory.join("broken.jar").exists());
        assert!(!session.directory.join("cancelled.jar").exists());
        no_partial(&session.directory);
    }

    #[test]
    fn final_publication_does_not_replace_a_target_created_during_copy() {
        struct RaceReader {
            bytes: Cursor<Vec<u8>>,
            target: PathBuf,
            created: bool,
        }
        impl Read for RaceReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.created {
                    fs::write(&self.target, b"concurrent file")?;
                    self.created = true;
                }
                self.bytes.read(buffer)
            }
        }
        let temp = crate::test_support::Workspace::new();
        let session = ModMutationSession::begin(&config(&temp.path), "test").unwrap();
        let target = session.directory.join("race.jar");
        let mut source = RaceReader {
            bytes: Cursor::new(jar()),
            target: target.clone(),
            created: false,
        };
        assert!(session
            .import_reader("race.jar", &mut source, &TaskControl::default())
            .is_err());
        assert_eq!(fs::read(target).unwrap(), b"concurrent file");
        no_partial(&session.directory);
    }

    #[test]
    fn running_instance_and_shared_game_directory_prevent_mod_mutation() {
        let temp = crate::test_support::Workspace::new();
        let config = config(&temp.path);
        let running =
            crate::instance_service::InstanceLaunchSession::begin(&config.root().unwrap(), "test")
                .unwrap();
        assert!(ModMutationSession::begin(&config, "test").is_err());
        drop(running);
        let held = DownloadLease::acquire(
            &config.instance_dir("other").unwrap().join("options.txt"),
            &TaskControl::default(),
            || {},
        )
        .unwrap();
        assert!(ModMutationSession::begin(&config, "test").is_err());
        drop(held);
        let session = ModMutationSession::begin(&config, "test").unwrap();
        fs::create_dir(session.directory.join("directory.jar")).unwrap();
        assert!(session.toggle("directory.jar").is_err());
        assert!(session.toggle("../unsafe.jar").is_err());
    }

    #[test]
    fn all_mod_write_entries_observe_shared_game_lease_before_changes_or_network() {
        let temp = crate::test_support::Workspace::new();
        let config = config(&temp.path);
        let root = config.root().unwrap();
        let mods = crate::mods::directory(&config, "test").unwrap();
        fs::write(mods.join("keep.jar"), b"original").unwrap();
        let held = DownloadLease::acquire(
            &config.instance_dir("other").unwrap().join("options.txt"),
            &TaskControl::default(),
            || {},
        )
        .unwrap();
        let busy = |error: anyhow::Error| {
            assert!(
                format!("{error:#}").contains("共享游戏目录正在使用"),
                "{error:#}"
            )
        };
        busy(crate::mods::remove(&config, "test", "keep.jar").unwrap_err());
        busy(crate::mods::restore(&config, "test", "no-backup").unwrap_err());
        let (tx, _rx) = std::sync::mpsc::channel();
        let backend = crate::backend::Backend::new(tx).unwrap();
        // No version metadata or network fixture: the shared lease must fail first.
        busy(
            backend
                .install_mod(&config, "test", "project", Some("version"))
                .unwrap_err(),
        );
        let mutation = InstanceMutation::begin(&root, "test").unwrap();
        busy(
            backend
                .repair_mod_inventory_with_session(&config, "test", &mutation)
                .unwrap_err(),
        );
        let receipt = root.join("versions/test").join(crate::pack_state::RECEIPT);
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::write(
            &receipt,
            serde_json::to_vec(&crate::pack_state::Receipt {
                schema: 1,
                instance: "test".into(),
                archive_sha1: "a".repeat(40),
                format: "mrpack".into(),
                files: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        busy(
            backend
                .repair_pack_files_with_session(&config, "test", &mutation)
                .unwrap_err(),
        );
        assert_eq!(fs::read(mods.join("keep.jar")).unwrap(), b"original");
        assert!(!temp.path.join("tui-mc-removed-mods").exists());
        let isolated = Config {
            isolated: true,
            ..config.clone()
        };
        let independent = ModMutationSession::begin(&isolated, "isolated").unwrap();
        drop(independent);
        drop(mutation);
        drop(held);
        let removed = crate::mods::remove(&config, "test", "keep.jar").unwrap();
        assert_eq!(fs::read(removed.join("keep.jar")).unwrap(), b"original");
    }
}
