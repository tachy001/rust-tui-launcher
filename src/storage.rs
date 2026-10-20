use anyhow::{Context, Result};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static SERIAL: AtomicU64 = AtomicU64::new(0);
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .context("文件路径没有名称")?
        .to_string_lossy();
    let temporary: PathBuf = parent.join(format!(
        ".{filename}.{}.{}.tmp",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path).context("无法替换目标文件；原文件已保留")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
pub fn atomic_copy(path: &Path, reader: &mut impl Read) -> Result<u64> {
    let parent = path.parent().context("目标文件没有父目录")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".tui-mc-copy-{}-{}.tmp",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<u64> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let count = std::io::copy(reader, &mut file)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(count)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replaces_existing_file() {
        let path = std::env::temp_dir().join(format!("tui-mc-atomic-{}", std::process::id()));
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_file(path).unwrap();
    }
}
