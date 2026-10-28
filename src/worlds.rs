use crate::{
    backend::{Backend, Message},
    config::Config,
    metadata::{safe_path, valid_id},
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static SERIAL: AtomicU64 = AtomicU64::new(0);
const MAX_FILES: usize = 500_000;
const MAX_BYTES: u64 = 256 * 1024 * 1024 * 1024;

struct WorldMutationSession {
    saves: PathBuf,
    _instance: crate::instance_service::InstanceMutation,
    _game: crate::download_lock::DownloadLease,
}
impl WorldMutationSession {
    fn begin(config: &Config, id: &str) -> Result<Self> {
        let root = config.root()?;
        let instance = crate::instance_service::InstanceMutation::begin(&root, id)?;
        let directory = config.effective(id)?.instance_dir(id)?;
        let game = crate::download_lock::DownloadLease::try_acquire(&directory.join("options.txt"))
            .context("游戏目录正在使用，无法安全备份或导入存档，请先关闭使用该目录的游戏或任务")?;
        Ok(Self {
            saves: directory.join("saves"),
            _instance: instance,
            _game: game,
        })
    }
}
struct OwnedBackup(Option<PathBuf>);
impl OwnedBackup {
    fn create(path: PathBuf) -> Result<(Self, fs::File)> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok((Self(Some(path)), file))
    }
    fn published(&mut self) {
        self.0 = None;
    }
}
impl Drop for OwnedBackup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn reject_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let linked = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        linked || metadata.file_attributes() & 0x400 != 0
    };
    if linked {
        bail!("存档操作不跟随链接：{}", path.display());
    }
    Ok(())
}
pub(crate) fn require_world(game_dir: &Path, name: &str) -> Result<()> {
    valid_id(name)?;
    let saves = game_dir.join("saves");
    let directory = saves.join(name);
    reject_link(&saves).context("无法打开实例存档目录")?;
    reject_link(&directory).context("所选存档不存在或不是普通目录")?;
    let level = directory.join("level.dat");
    reject_link(&level).context("所选存档缺少 level.dat")?;
    if !directory.is_dir() || !level.is_file() {
        bail!("所选目录不是有效存档");
    }
    Ok(())
}
#[cfg(test)]
mod quick_play_tests {
    use super::*;
    #[test]
    fn existing_named_world_is_required_and_never_created_by_preflight() {
        let temp = crate::test_support::Workspace::new();
        assert!(require_world(&temp.path, "missing").is_err());
        assert!(!temp.path.join("saves").exists());
        let world = temp.path.join("saves/中文 World");
        fs::create_dir_all(&world).unwrap();
        assert!(require_world(&temp.path, "中文 World").is_err());
        fs::write(world.join("level.dat"), b"fixture, not a real NBT world").unwrap();
        require_world(&temp.path, "中文 World").unwrap();
        assert!(require_world(&temp.path, "../escape").is_err());
    }
}
pub fn list(config: &Config, id: &str) -> Result<Vec<String>> {
    let saves = config.effective(id)?.instance_path(id)?.join("saves");
    match fs::symlink_metadata(&saves) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error).context("无法读取存档目录"),
    }
    reject_link(&saves)?;
    let mut result = vec![];
    for entry in fs::read_dir(&saves)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && reject_link(&entry.path()).is_ok()
            && entry.path().join("level.dat").is_file()
        {
            result.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    result.sort_by_key(|name| name.to_lowercase());
    Ok(result)
}
fn scan(
    dir: &Path,
    base: &Path,
    depth: usize,
    files: &mut Vec<(PathBuf, String)>,
    bytes: &mut u64,
    control: &TaskControl,
) -> Result<()> {
    control.check()?;
    if depth > 64 {
        bail!("存档目录层级过深");
    }
    reject_link(dir)?;
    for entry in fs::read_dir(dir)? {
        control.check()?;
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == crate::download_lock::LOCK_DIR {
            continue;
        }
        reject_link(&path)?;
        if entry.file_type()?.is_dir() {
            scan(&path, base, depth + 1, files, bytes, control)?;
        } else if entry.file_type()?.is_file() {
            if entry.file_name() == "session.lock" {
                continue;
            }
            let name = path
                .strip_prefix(base)?
                .to_string_lossy()
                .replace('\\', "/");
            safe_path(base, &name)?;
            *bytes = bytes
                .checked_add(entry.metadata()?.len())
                .context("存档大小溢出")?;
            files.push((path, name));
            if files.len() > MAX_FILES || *bytes > MAX_BYTES {
                bail!("存档超过备份上限（50 万文件 / 256 GiB）");
            }
        } else {
            bail!("存档包含不支持的文件类型");
        }
    }
    Ok(())
}
fn copy_controlled(
    reader: &mut impl Read,
    writer: &mut impl Write,
    control: &TaskControl,
) -> Result<u64> {
    let mut buffer = [0; 65536];
    let mut size = 0u64;
    loop {
        control.check()?;
        let n = match reader.read(&mut buffer) {
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        control.check()?;
        if n == 0 {
            break;
        }
        size = size.checked_add(n as u64).context("存档复制大小溢出")?;
        if size > MAX_BYTES {
            bail!("解压 / 备份文件超过大小上限");
        }
        writer.write_all(&buffer[..n])?;
    }
    Ok(size)
}
impl Backend {
    pub fn backup_world(
        &self,
        config: &Config,
        id: &str,
        world: &str,
        output: &Path,
    ) -> Result<()> {
        self.control.check()?;
        valid_id(id)?;
        valid_id(world)?;
        let session = WorldMutationSession::begin(config, id)?;
        let saves = session.saves.clone();
        reject_link(&saves)?;
        let source = safe_path(&saves, world)?;
        reject_link(&source)?;
        if !source.join("level.dat").is_file() {
            bail!("所选目录不是包含 level.dat 的世界");
        }
        let session_lock =
            crate::world_lock_service::WorldSessionLock::existing(&source.join("session.lock"))?;
        let mut files = vec![];
        let mut bytes = 0;
        scan(&source, &source, 0, &mut files, &mut bytes, &self.control)?;
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let output_parent = fs::canonicalize(parent)?;
        if output_parent.starts_with(fs::canonicalize(&source)?) {
            bail!("备份文件不能放入正在备份的世界目录内");
        }
        if output.exists() {
            bail!("目标 ZIP 已存在，不会覆盖");
        }
        let temporary = parent.join(format!(
            ".tui-mc-world-{}-{}.tmp",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let same_name = if cfg!(windows) {
            temporary
                .file_name()
                .unwrap()
                .eq_ignore_ascii_case(output.file_name().context("备份路径缺少文件名")?)
        } else {
            temporary.file_name() == output.file_name()
        };
        if same_name {
            bail!("备份文件名与内部暂存名冲突，请选择其他输出名称");
        }
        let (mut temporary_owner, file) = OwnedBackup::create(temporary.clone())?;
        let result = (|| -> Result<()> {
            let mut archive = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let total = files.len();
            for (index, (path, name)) in files.into_iter().enumerate() {
                archive.start_file(name, options)?;
                copy_controlled(&mut fs::File::open(path)?, &mut archive, &self.control)?;
                let _ = self
                    .tx
                    .send(Message::Progress("存档备份".into(), index + 1, total));
            }
            archive.finish()?.sync_all()?;
            self.control.check()?;
            crate::publication_service::NoReplacePublisher
                .publish(&temporary, output)
                .context("无法发布完整备份；目标可能已存在，或文件系统不支持无覆盖移动")?;
            temporary_owner.published();
            Ok(())
        })();
        drop(session_lock);
        result
    }
    pub fn import_world(&self, config: &Config, id: &str, input: &Path, name: &str) -> Result<()> {
        self.control.check()?;
        valid_id(id)?;
        valid_id(name)?;
        let session = WorldMutationSession::begin(config, id)?;
        let saves = session.saves.clone();
        fs::create_dir_all(&saves)?;
        reject_link(&saves)?;
        let dest = safe_path(&saves, name)?;
        if dest.exists() {
            bail!("同名世界已存在，请使用新的存档目录名");
        }
        let mut archive = zip::ZipArchive::new(fs::File::open(input)?)?;
        if archive.len() > MAX_FILES {
            bail!("ZIP 文件数量超过上限");
        }
        let mut names = HashSet::new();
        let mut members = vec![];
        let mut total = 0u64;
        let mut roots = vec![];
        for index in 0..archive.len() {
            self.control.check()?;
            let entry = archive.by_index(index)?;
            let path = entry.name().replace('\\', "/");
            safe_path(&saves, path.trim_end_matches('/'))?;
            if entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                bail!("存档 ZIP 不能包含符号链接");
            }
            if entry.is_dir() {
                continue;
            }
            if !names.insert(path.to_lowercase()) {
                bail!("ZIP 包含重复或大小写冲突的路径");
            }
            if path == "level.dat" {
                roots.push(String::new());
            } else if let Some(prefix) = path.strip_suffix("/level.dat") {
                roots.push(format!("{prefix}/"));
            }
            total = total.checked_add(entry.size()).context("ZIP 大小溢出")?;
            if total > MAX_BYTES {
                bail!("ZIP 解压大小超过 256 GiB 上限");
            }
            members.push((index, path, entry.size()));
        }
        if roots.len() != 1 {
            bail!("ZIP 必须包含且仅包含一个世界的 level.dat");
        }
        let prefix = &roots[0];
        let staging = saves.join(format!(
            ".tui-mc-import-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&staging)?;
        // Use a flat list of created paths for cleanup, without recursive deletion.
        let mut created = vec![];
        let result = (|| -> Result<()> {
            let count = members.len();
            for (index, (member, path, size)) in members.iter().enumerate() {
                self.control.check()?;
                let Some(relative) = path.strip_prefix(prefix) else {
                    continue;
                };
                if relative == "session.lock" {
                    continue;
                }
                let target = safe_path(&staging, relative)?;
                fs::create_dir_all(target.parent().context("存档文件缺少目录")?)?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                created.push(target.clone());
                let copied = copy_controlled(
                    &mut archive.by_index(*member)?.take(size.saturating_add(1)),
                    &mut file,
                    &self.control,
                )?;
                if copied != *size {
                    bail!("ZIP 文件长度不匹配");
                }
                file.sync_all()?;
                let _ = self
                    .tx
                    .send(Message::Progress("存档导入".into(), index + 1, count));
            }
            self.control.check()?;
            if dest.exists() {
                bail!("目标世界已出现，取消覆盖");
            }
            crate::publication_service::NoReplacePublisher
                .publish(&staging, &dest)
                .context("无法发布存档；目标可能已存在，或文件系统不支持无覆盖移动")?;
            Ok(())
        })();
        if result.is_err() {
            for path in created.into_iter().rev() {
                let _ = fs::remove_file(&path);
                let mut parent = path.parent();
                while let Some(dir) =
                    parent.filter(|dir| *dir != staging && dir.starts_with(&staging))
                {
                    let _ = fs::remove_dir(dir);
                    parent = dir.parent();
                }
            }
            let _ = fs::remove_dir(&staging);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn backend() -> Backend {
        let (tx, _) = std::sync::mpsc::channel();
        Backend::new(tx).unwrap()
    }
    #[test]
    fn backup_honors_world_session_lock_before_creating_output() {
        let temp = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: temp.path.clone(),
            isolated: false,
            ..Default::default()
        };
        let world = temp.path.join("saves/world");
        fs::create_dir_all(&world).unwrap();
        fs::write(world.join("level.dat"), b"fixture").unwrap();
        let lock_path = world.join("session.lock");
        fs::write(&lock_path, b"unchanged lock content").unwrap();
        let lock = crate::world_lock_service::WorldSessionLock::existing(&lock_path)
            .unwrap()
            .unwrap();
        let output = temp.path.join("backup.zip");
        assert!(backend()
            .backup_world(&config, "test", "world", &output)
            .is_err());
        assert!(!output.exists());
        drop(lock);
        backend()
            .backup_world(&config, "test", "world", &output)
            .unwrap();
        assert!(output.is_file());
        assert_eq!(fs::read(lock_path).unwrap(), b"unchanged lock content");
    }
    #[test]
    fn shared_game_directory_blocks_world_operations_before_files_are_created() {
        let temp = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: temp.path.clone(),
            isolated: false,
            ..Default::default()
        };
        let backend = backend();
        let game = crate::download_lock::DownloadLease::try_acquire(&temp.path.join("options.txt"))
            .unwrap();
        let output = temp.path.join("backup.zip");
        let missing = temp.path.join("missing.zip");
        let error = backend
            .import_world(&config, "other", &missing, "world")
            .unwrap_err();
        assert!(error.to_string().contains("游戏目录正在使用"));
        assert!(backend
            .backup_world(&config, "different", "world", &output)
            .unwrap_err()
            .to_string()
            .contains("游戏目录正在使用"));
        assert!(!temp.path.join("saves").exists());
        assert!(!output.exists());
        drop(game);
        let session = WorldMutationSession::begin(&config, "other").unwrap();
        assert!(
            crate::download_lock::DownloadLease::try_acquire(&temp.path.join("options.txt"))
                .is_err()
        );
        drop(session);
        crate::download_lock::DownloadLease::try_acquire(&temp.path.join("options.txt")).unwrap();
    }
    #[test]
    fn backup_temporary_cleanup_requires_successful_creation() {
        let temp = crate::test_support::Workspace::new();
        let path = temp.path.join("existing.tmp");
        fs::write(&path, b"foreign data").unwrap();
        assert!(OwnedBackup::create(path.clone()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"foreign data");
        let path = temp.path.join("owned.tmp");
        let (owned, mut file) = OwnedBackup::create(path.clone()).unwrap();
        file.write_all(b"private data").unwrap();
        drop(file);
        drop(owned);
        assert!(!path.exists());
        let (mut owned, file) = OwnedBackup::create(path.clone()).unwrap();
        drop(file);
        crate::publication_service::NoReplacePublisher
            .publish(&path, &temp.path.join("published.zip"))
            .unwrap();
        owned.published();
        fs::write(&path, b"new foreign file").unwrap();
        drop(owned);
        assert_eq!(fs::read(&path).unwrap(), b"new foreign file");
    }
    #[test]
    fn controlled_copy_retries_interruption_and_checks_cancellation_after_read_or_eof() {
        struct Reader<'a> {
            control: &'a TaskControl,
            data: std::io::Cursor<Vec<u8>>,
            interrupt: bool,
            cancel: bool,
        }
        impl Read for Reader<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if std::mem::take(&mut self.interrupt) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let size = self.data.read(output)?;
                if self.cancel {
                    self.control.cancel();
                }
                Ok(size)
            }
        }
        let control = TaskControl::default();
        let mut reader = Reader {
            control: &control,
            data: std::io::Cursor::new(b"world data".to_vec()),
            interrupt: true,
            cancel: false,
        };
        let mut output = vec![];
        assert_eq!(
            copy_controlled(&mut reader, &mut output, &control).unwrap(),
            10
        );
        assert_eq!(output, b"world data");
        for data in [vec![], b"world data".to_vec()] {
            let control = TaskControl::default();
            let mut reader = Reader {
                control: &control,
                data: std::io::Cursor::new(data),
                interrupt: false,
                cancel: true,
            };
            let mut output = vec![];
            assert!(copy_controlled(&mut reader, &mut output, &control).is_err());
            assert!(output.is_empty());
        }
    }
    #[test]
    fn backup_and_import_preserve_world_without_overwriting_existing_data() {
        let temp = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: temp.path.clone(),
            isolated: false,
            ..Config::default()
        };
        let source = temp.path.join("saves/原世界");
        fs::create_dir_all(source.join("region")).unwrap();
        fs::write(source.join("level.dat"), b"level-data").unwrap();
        fs::write(source.join("region/r.0.0.mca"), b"chunks").unwrap();
        fs::write(source.join("session.lock"), b"lock").unwrap();
        let output = temp.path.join("backup.zip");
        let backend = backend();
        backend
            .backup_world(&config, "test", "原世界", &output)
            .unwrap();
        let mut archive = zip::ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
        assert!(archive.by_name("level.dat").is_ok());
        assert!(archive.by_name("session.lock").is_err());
        drop(archive);
        backend
            .import_world(&config, "test", &output, "恢复世界")
            .unwrap();
        assert_eq!(
            fs::read(temp.path.join("saves/恢复世界/region/r.0.0.mca")).unwrap(),
            b"chunks"
        );
        assert_eq!(list(&config, "test").unwrap().len(), 2);
        fs::write(temp.path.join("saves/恢复世界/level.dat"), b"new-data").unwrap();
        assert!(backend
            .import_world(&config, "test", &output, "恢复世界")
            .is_err());
        assert_eq!(
            fs::read(temp.path.join("saves/恢复世界/level.dat")).unwrap(),
            b"new-data"
        );
        assert!(backend
            .backup_world(&config, "test", "原世界", &output)
            .is_err());
        backend.control.cancel();
        assert!(backend
            .import_world(&config, "test", &output, "cancelled")
            .is_err());
        assert!(!temp.path.join("saves/cancelled").exists());
    }
    #[test]
    fn import_supports_one_wrapping_folder_and_rejects_zip_traversal() {
        let temp = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: temp.path.clone(),
            isolated: false,
            ..Config::default()
        };
        let input = temp.path.join("world.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&input).unwrap());
        zip.start_file(
            "wrapper/level.dat",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"level").unwrap();
        zip.finish().unwrap();
        backend()
            .import_world(&config, "test", &input, "wrapped")
            .unwrap();
        assert_eq!(
            fs::read(temp.path.join("saves/wrapped/level.dat")).unwrap(),
            b"level"
        );
        let mut zip = zip::ZipWriter::new(fs::File::create(&input).unwrap());
        zip.start_file("level.dat", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"level").unwrap();
        zip.start_file("../outside", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"bad").unwrap();
        zip.finish().unwrap();
        assert!(backend()
            .import_world(&config, "test", &input, "bad")
            .is_err());
        assert!(!temp.path.join("saves/bad").exists());
        assert!(!temp.path.join("outside").exists());
    }
}
