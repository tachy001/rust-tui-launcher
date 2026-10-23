use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct ResumeRecord {
    schema: u32,
    source: String,
    etag: String,
}
pub(crate) struct ResumeBinding {
    path: PathBuf,
}
impl ResumeBinding {
    pub(crate) fn new(partial: &Path) -> Self {
        let mut path = partial.as_os_str().to_owned();
        path.push(".resume.json");
        Self { path: path.into() }
    }
    pub(crate) fn validator(&self, url: &str) -> Option<String> {
        let metadata = fs::symlink_metadata(&self.path).ok()?;
        if !metadata.file_type().is_file() || metadata.len() > 16384 {
            return None;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return None;
            }
        }
        let mut bytes = vec![];
        fs::File::open(&self.path)
            .ok()?
            .take(16385)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 16384 {
            return None;
        }
        let record: ResumeRecord = serde_json::from_slice(&bytes).ok()?;
        (record.schema == 1 && record.source == source_key(url) && strong_etag(&record.etag))
            .then_some(record.etag)
    }
    pub(crate) fn record(&self, url: &str, etag: Option<&str>) -> Result<()> {
        if let Some(etag) = etag.filter(|etag| strong_etag(etag)) {
            crate::storage::atomic_write(
                &self.path,
                &serde_json::to_vec(&ResumeRecord {
                    schema: 1,
                    source: source_key(url),
                    etag: etag.into(),
                })?,
            )?;
        } else {
            self.invalidate()?;
        }
        Ok(())
    }
    pub(crate) fn clear(&self) {
        let _ = self.invalidate();
    }
    pub(crate) fn invalidate(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
fn source_key(url: &str) -> String {
    format!("{:x}", Sha1::digest(url.as_bytes()))
}
fn strong_etag(etag: &str) -> bool {
    etag.len() <= 4096
        && etag.starts_with('"')
        && etag.ends_with('"')
        && etag.len() >= 2
        && etag.as_bytes()[1..etag.len() - 1]
            .iter()
            .all(|byte| *byte == 0x21 || (0x23..=0x7e).contains(byte))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strong_validator_is_bound_to_source_without_storing_url_credentials() {
        let temp = crate::test_support::Workspace::new();
        let binding = ResumeBinding::new(&temp.path.join("file.part"));
        let url = "https://example.invalid/file?private=do-not-store-this";
        binding.record(url, Some("\"version-one\"")).unwrap();
        assert_eq!(binding.validator(url).as_deref(), Some("\"version-one\""));
        assert!(binding.validator("https://mirror.invalid/file").is_none());
        assert!(!fs::read_to_string(&binding.path)
            .unwrap()
            .contains("do-not-store-this"));
        binding.clear();
        assert!(!binding.path.exists());
    }
    #[test]
    fn weak_invalid_missing_and_oversized_metadata_cannot_enable_resume() {
        for value in [
            "W/\"weak\"",
            "unquoted",
            "\"bad\nvalue\"",
            "\"a\"b\"",
            "\"é\"",
        ] {
            assert!(!strong_etag(value));
        }
        assert!(strong_etag("\"\""));
        let temp = crate::test_support::Workspace::new();
        let binding = ResumeBinding::new(&temp.path.join("file.part"));
        binding.record("url", Some("\"v\"")).unwrap();
        binding.record("url", Some("W/\"weak\"")).unwrap();
        assert!(binding.validator("url").is_none());
        fs::write(&binding.path, b"broken json").unwrap();
        assert!(binding.validator("url").is_none());
        fs::write(&binding.path, vec![b'x'; 16385]).unwrap();
        assert!(binding.validator("url").is_none());
        fs::remove_file(&binding.path).unwrap();
        fs::create_dir(&binding.path).unwrap();
        assert!(binding.record("url", None).is_err());
        assert!(binding.path.is_dir());
    }
}
