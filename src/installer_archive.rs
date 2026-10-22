use crate::task::TaskControl;
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// zip 2.x exposes a map of decoded names, which hides duplicate directory
/// entries. Compare it to the actual central record count before trusting it.
pub(crate) fn open(path: &Path, control: &TaskControl) -> Result<zip::ZipArchive<fs::File>> {
    control.check()?;
    let file = fs::File::open(path)?;
    let mut directory = file.try_clone()?;
    let archive = zip::ZipArchive::new(file).context("安装器 ZIP 目录无效")?;
    let length = directory.metadata()?.len();
    directory.seek(SeekFrom::Start(archive.central_directory_start()))?;
    let mut count = 0;
    loop {
        control.check()?;
        let mut signature = [0; 4];
        directory
            .read_exact(&mut signature)
            .context("安装器 ZIP 目录被截断")?;
        if signature != *b"PK\x01\x02" {
            if !matches!(&signature, b"PK\x05\x06" | b"PK\x06\x06" | b"PK\x05\x05") {
                bail!("安装器 ZIP 目录终止标记无效");
            }
            break;
        }
        let mut header = [0; 42];
        directory.read_exact(&mut header)?;
        let name = u16::from_le_bytes([header[24], header[25]]);
        let extra = u16::from_le_bytes([header[26], header[27]]);
        let comment = u16::from_le_bytes([header[28], header[29]]);
        let end = directory
            .stream_position()?
            .checked_add(u64::from(name) + u64::from(extra) + u64::from(comment))
            .context("安装器目录偏移溢出")?;
        if end > length {
            bail!("安装器 ZIP 目录记录越界");
        }
        directory.seek(SeekFrom::Start(end))?;
        count += 1;
        if count > 100_000 {
            bail!("安装器 ZIP 条目超过数量限制");
        }
    }
    if count != archive.len() {
        bail!("安装器 ZIP 有重复名称或目录记录不一致");
    }
    control.check()?;
    Ok(archive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn rejects_duplicates_that_the_zip_library_name_map_would_hide() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("duplicate.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        for name in ["first.class", "other.class"] {
            archive
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(b"fixture").unwrap();
        }
        archive.finish().unwrap();
        let mut bytes = fs::read(&path).unwrap();
        for start in 0..=bytes.len() - b"other.class".len() {
            if &bytes[start..start + 11] == b"other.class" {
                bytes[start..start + 11].copy_from_slice(b"first.class");
            }
        }
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            zip::ZipArchive::new(fs::File::open(&path).unwrap())
                .unwrap()
                .len(),
            1
        );
        assert!(open(&path, &TaskControl::default())
            .err()
            .unwrap()
            .to_string()
            .contains("重复"));
    }
    #[test]
    fn valid_archives_with_zip64_entry_extras_and_comments_are_accepted() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("valid.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        archive.set_comment("PK markers in a comment are not directory records");
        archive
            .start_file(
                "entry",
                zip::write::SimpleFileOptions::default().large_file(true),
            )
            .unwrap();
        archive.write_all(b"fixture").unwrap();
        archive.finish().unwrap();
        assert_eq!(open(&path, &TaskControl::default()).unwrap().len(), 1);
        let control = TaskControl::default();
        control.cancel();
        assert!(open(Path::new("missing"), &control)
            .err()
            .unwrap()
            .to_string()
            .contains("取消"));
    }
}
