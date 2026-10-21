use crate::{
    backend::{Backend, Message},
    config::Config,
    instances::InstanceLock,
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
pub fn list(config: &Config, id: &str) -> Result<Vec<String>> {
    let saves = config.effective(id).instance_dir(id)?.join("saves");
    if !saves.exists() {
        return Ok(vec![]);
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
    let mut size = 0;
    loop {
        control.check()?;
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size += n as u64;
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
        let _lock = InstanceLock::acquire(&config.root()?.join("versions").join(id))?;
        let saves = config.effective(id).instance_dir(id)?.join("saves");
        reject_link(&saves)?;
        let source = safe_path(&saves, world)?;
        reject_link(&source)?;
        if !source.join("level.dat").is_file() {
            bail!("所选目录不是包含 level.dat 的世界");
        }
        // Also honor the world's own lock where the platform exposes compatible locks.
        let session_lock = if source.join("session.lock").exists() {
            reject_link(&source.join("session.lock"))?;
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(source.join("session.lock"))?;
            file.try_lock()
                .context("世界可能正在游戏中使用，请先退出该世界")?;
            Some(file)
        } else {
            None
        };
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
        let result = (|| -> Result<()> {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
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
            // Same-directory hard links publish atomically without copying large ZIPs twice.
            if fs::hard_link(&temporary, output).is_ok() {
                return Ok(());
            }
            // Filesystems without hard links use create_new to preserve an existing target.
            let mut destination = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            let commit = copy_controlled(
                &mut fs::File::open(&temporary)?,
                &mut destination,
                &self.control,
            )
            .and_then(|_| Ok(destination.sync_all()?));
            if commit.is_err() {
                drop(destination);
                let _ = fs::remove_file(output);
            }
            commit?;
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        drop(session_lock);
        result
    }
    pub fn import_world(&self, config: &Config, id: &str, input: &Path, name: &str) -> Result<()> {
        self.control.check()?;
        valid_id(id)?;
        valid_id(name)?;
        let _lock = InstanceLock::acquire(&config.root()?.join("versions").join(id))?;
        let saves = config.effective(id).instance_dir(id)?.join("saves");
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
            if dest.exists() {
                bail!("目标世界已出现，取消覆盖");
            }
            fs::rename(&staging, &dest)?;
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
