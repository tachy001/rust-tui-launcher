use crate::{backend::Backend, task::TaskControl};
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) const MAX_METADATA: usize = 16 * 1024 * 1024;
pub(crate) fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    if url.len() > 8192
        || url.chars().any(char::is_control)
        || parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        bail!("元数据地址必须为不含凭证或片段的 HTTPS URL");
    }
    Ok(())
}
fn check_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_METADATA {
        bail!("元数据超过 16 MiB 限制");
    }
    Ok(())
}
pub(crate) trait MetadataSource {
    fn candidates(&self, url: &str) -> Vec<String> {
        vec![url.into()]
    }
    fn fetch(&self, url: &str) -> Result<Vec<u8>>;
}
pub(crate) trait MetadataStore {
    fn read(&self, url: &str) -> Result<Option<Vec<u8>>>;
    fn write(&self, url: &str, bytes: &[u8]) -> Result<()>;
}
pub(crate) struct FileMetadataStore {
    root: PathBuf,
}
impl FileMetadataStore {
    pub(crate) fn new(root: &Path) -> Self {
        Self { root: root.into() }
    }
    fn path(&self, url: &str) -> PathBuf {
        self.root
            .join("metadata")
            .join(format!("{:x}.cache", Sha1::digest(url.as_bytes())))
    }
    fn header(url: &str) -> String {
        format!("TUI-METADATA-1\n{url}\n")
    }
    fn read_file(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("读取元数据缓存失败"),
        };
        if file.metadata()?.len() > limit as u64 {
            bail!("元数据缓存超过读取限制");
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            bail!("元数据缓存超过读取限制");
        }
        Ok(Some(bytes))
    }
}
impl MetadataStore for FileMetadataStore {
    fn read(&self, url: &str) -> Result<Option<Vec<u8>>> {
        validate_url(url)?;
        let header = Self::header(url);
        if let Some(bytes) = Self::read_file(&self.path(url), MAX_METADATA + header.len())? {
            let body = bytes
                .strip_prefix(header.as_bytes())
                .context("缓存地址或格式不匹配")?;
            check_size(body)?;
            return Ok(Some(body.into()));
        }
        // Upgrade compatibility: the old manifest cache had no URL envelope.
        if url == crate::metadata::MANIFEST {
            return Self::read_file(&self.root.join("versions.json"), MAX_METADATA);
        }
        Ok(None)
    }
    fn write(&self, url: &str, bytes: &[u8]) -> Result<()> {
        validate_url(url)?;
        check_size(bytes)?;
        let mut data = Self::header(url).into_bytes();
        data.extend_from_slice(bytes);
        crate::storage::atomic_write(&self.path(url), &data)
    }
}

pub(crate) struct MetadataResult<T> {
    pub(crate) value: T,
    pub(crate) warning: Option<String>,
}
/// Network / schema / persistence policy is independent of HTTP and filesystem IO.
pub(crate) struct MetadataCache<'a> {
    source: &'a dyn MetadataSource,
    store: &'a dyn MetadataStore,
    control: &'a TaskControl,
}
impl<'a> MetadataCache<'a> {
    pub(crate) fn new(
        source: &'a dyn MetadataSource,
        store: &'a dyn MetadataStore,
        control: &'a TaskControl,
    ) -> Self {
        Self {
            source,
            store,
            control,
        }
    }
    pub(crate) fn load<T>(
        &self,
        url: &str,
        parse: impl Fn(&[u8]) -> Result<T>,
    ) -> Result<MetadataResult<T>> {
        validate_url(url)?;
        self.control.check()?;
        let mut online = Err(anyhow::anyhow!("没有可用元数据来源"));
        for candidate in self.source.candidates(url) {
            self.control.check()?;
            online = validate_url(&candidate)
                .and_then(|_| self.source.fetch(&candidate))
                .and_then(|bytes| {
                    self.control.check()?;
                    check_size(&bytes)?;
                    let value = parse(&bytes)?;
                    Ok((bytes, value))
                });
            self.control.check()?;
            if online.is_ok() {
                break;
            }
        }
        self.control.check()?;
        match online {
            Ok((bytes, value)) => {
                let warning = self
                    .store
                    .write(url, &bytes)
                    .err()
                    .map(|error| format!("已获取实时元数据，但保存缓存失败：{error:#}"));
                self.control.check()?;
                Ok(MetadataResult { value, warning })
            }
            Err(network_error) => {
                let cached = self.store.read(url);
                self.control.check()?;
                let cached = cached
                    .and_then(|data| data.context("没有可用元数据缓存"))
                    .and_then(|bytes| {
                        check_size(&bytes)?;
                        parse(&bytes)
                    });
                self.control.check()?;
                match cached {
                    Ok(value) => Ok(MetadataResult {
                        value,
                        warning: Some(format!(
                            "无法获取有效实时元数据，使用已校验缓存：{network_error:#}"
                        )),
                    }),
                    Err(cache_error) => Err(network_error).with_context(|| {
                        format!("实时元数据与缓存均不可用；缓存原因：{cache_error:#}")
                    }),
                }
            }
        }
    }
}

impl MetadataSource for Backend {
    fn candidates(&self, url: &str) -> Vec<String> {
        crate::sources::urls(url, &self.source)
    }
    fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        validate_url(url)?;
        self.control.check()?;
        let mut response = self.client.get(url).send()?.error_for_status()?;
        self.control.check()?;
        if response.url().scheme() != "https" {
            bail!("元数据被重定向到非 HTTPS 地址");
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_METADATA as u64)
        {
            bail!("元数据超过 16 MiB 限制");
        }
        read_response(&mut response, &self.control)
    }
}

fn read_response(reader: &mut impl Read, control: &TaskControl) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        control.check()?;
        let limit = buffer.len().min(MAX_METADATA + 1 - bytes.len());
        let count = match reader.read(&mut buffer[..limit]) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("读取元数据响应失败"),
        };
        control.check()?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        check_size(&bytes)?;
    }
    Ok(bytes)
}
impl Backend {
    // Deliberately opt-in: authentication requests and credentials are never routed here.
    pub(crate) fn cached_metadata<T>(
        &self,
        url: &str,
        parse: impl Fn(&[u8]) -> Result<T>,
    ) -> Result<T> {
        let store = FileMetadataStore::new(Path::new(".tui-mc/cache"));
        let result = MetadataCache::new(self, &store, &self.control).load(url, parse)?;
        if let Some(warning) = result.warning {
            self.log(warning);
        }
        Ok(result.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    const URL: &str = "https://example.invalid/catalog";
    #[test]
    fn response_read_retries_interruption_but_stops_when_cancelled() {
        struct InterruptedReader<'a> {
            control: &'a TaskControl,
            cancel: bool,
            calls: usize,
            body: std::io::Cursor<Vec<u8>>,
        }
        impl Read for InterruptedReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                if self.calls == 1 {
                    if self.cancel {
                        self.control.cancel();
                    }
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.body.read(buffer)
            }
        }
        for cancel in [false, true] {
            let control = TaskControl::default();
            let mut reader = InterruptedReader {
                control: &control,
                cancel,
                calls: 0,
                body: std::io::Cursor::new(b"metadata".to_vec()),
            };
            let result = read_response(&mut reader, &control);
            if cancel {
                assert!(result.is_err());
                assert_eq!(reader.calls, 1);
            } else {
                assert_eq!(result.unwrap(), b"metadata");
                assert_eq!(reader.calls, 3);
            }
        }
    }
    #[derive(Default)]
    struct Store {
        bytes: RefCell<Option<Vec<u8>>>,
        writes: Cell<usize>,
        reads: Cell<usize>,
        fail_write: bool,
        fail_read: bool,
    }
    impl MetadataStore for Store {
        fn read(&self, _: &str) -> Result<Option<Vec<u8>>> {
            self.reads.set(self.reads.get() + 1);
            if self.fail_read {
                bail!("cache inaccessible");
            }
            Ok(self.bytes.borrow().clone())
        }
        fn write(&self, _: &str, bytes: &[u8]) -> Result<()> {
            self.writes.set(self.writes.get() + 1);
            if self.fail_write {
                bail!("cache read-only");
            }
            *self.bytes.borrow_mut() = Some(bytes.into());
            Ok(())
        }
    }
    struct Source {
        bytes: Option<Vec<u8>>,
        calls: Cell<usize>,
    }
    impl MetadataSource for Source {
        fn fetch(&self, _: &str) -> Result<Vec<u8>> {
            self.calls.set(self.calls.get() + 1);
            self.bytes.clone().context("offline")
        }
    }
    fn source(bytes: Option<&[u8]>) -> Source {
        Source {
            bytes: bytes.map(Into::into),
            calls: Cell::new(0),
        }
    }
    fn parse(bytes: &[u8]) -> Result<u64> {
        serde_json::from_slice(bytes).context("invalid catalog")
    }
    #[test]
    fn live_value_wins_even_when_cache_is_read_only() {
        let store = Store {
            fail_write: true,
            ..Default::default()
        };
        let result = MetadataCache::new(&source(Some(b"7")), &store, &TaskControl::default())
            .load(URL, parse)
            .unwrap();
        assert_eq!(result.value, 7);
        assert!(result.warning.unwrap().contains("cache read-only"));
        assert_eq!(store.reads.get(), 0);
    }
    #[test]
    fn network_failure_or_malformed_response_uses_valid_cache_without_overwriting() {
        for bytes in [None, Some(b"bad".as_slice())] {
            let store = Store {
                bytes: RefCell::new(Some(b"3".to_vec())),
                ..Default::default()
            };
            let result = MetadataCache::new(&source(bytes), &store, &TaskControl::default())
                .load(URL, parse)
                .unwrap();
            assert_eq!(result.value, 3);
            assert!(result.warning.is_some());
            assert_eq!(store.writes.get(), 0);
        }
    }
    #[test]
    fn bad_cache_preserves_both_failure_causes() {
        for store in [
            Store {
                fail_read: true,
                ..Default::default()
            },
            Store {
                bytes: RefCell::new(Some(b"bad".to_vec())),
                ..Default::default()
            },
        ] {
            let error = MetadataCache::new(&source(None), &store, &TaskControl::default())
                .load(URL, parse)
                .err()
                .unwrap();
            let message = format!("{error:#}");
            assert!(message.contains("offline"));
            assert!(message.contains("缓存"));
        }
    }
    #[test]
    fn mirror_schema_failure_continues_to_official_before_using_cache() {
        struct Mirrored(Cell<usize>);
        impl MetadataSource for Mirrored {
            fn candidates(&self, url: &str) -> Vec<String> {
                vec!["https://mirror.invalid/catalog".into(), url.into()]
            }
            fn fetch(&self, url: &str) -> Result<Vec<u8>> {
                self.0.set(self.0.get() + 1);
                Ok(if url == URL {
                    b"9".to_vec()
                } else {
                    b"<html>error</html>".to_vec()
                })
            }
        }
        let source = Mirrored(Cell::new(0));
        let store = Store::default();
        let result = MetadataCache::new(&source, &store, &TaskControl::default())
            .load(URL, parse)
            .unwrap();
        assert_eq!(result.value, 9);
        assert_eq!(source.0.get(), 2);
        assert_eq!(store.reads.get(), 0);
        assert_eq!(store.bytes.borrow().as_deref(), Some(b"9".as_slice()));
    }
    #[test]
    fn cancellation_never_returns_cached_data_or_runs_parser() {
        struct Cancelling<'a>(&'a TaskControl);
        impl MetadataSource for Cancelling<'_> {
            fn fetch(&self, _: &str) -> Result<Vec<u8>> {
                self.0.cancel();
                Ok(b"9".to_vec())
            }
        }
        let control = TaskControl::default();
        let store = Store::default();
        let parsed = Cell::new(0);
        assert!(MetadataCache::new(&Cancelling(&control), &store, &control)
            .load(URL, |bytes| {
                parsed.set(parsed.get() + 1);
                parse(bytes)
            })
            .is_err());
        assert_eq!(store.reads.get(), 0);
        assert_eq!(store.writes.get(), 0);
        assert_eq!(parsed.get(), 0);
    }
    #[test]
    fn invalid_requests_and_oversized_payloads_do_not_replace_cache() {
        let store = Store {
            bytes: RefCell::new(Some(b"3".to_vec())),
            ..Default::default()
        };
        let remote = source(None);
        for url in [
            "http://example.invalid/x",
            "https://user:password@example.invalid/x",
            "https://example.invalid/x#fragment",
        ] {
            assert!(MetadataCache::new(&remote, &store, &TaskControl::default())
                .load(url, parse)
                .is_err());
        }
        assert_eq!(remote.calls.get(), 0);
        let remote = Source {
            bytes: Some(vec![b'1'; MAX_METADATA + 1]),
            calls: Cell::new(0),
        };
        assert_eq!(
            MetadataCache::new(&remote, &store, &TaskControl::default())
                .load(URL, parse)
                .unwrap()
                .value,
            3
        );
        assert_eq!(store.writes.get(), 0);
    }
    #[test]
    fn file_store_reads_legacy_manifest_and_rejects_wrong_url_envelope() {
        let workspace = crate::test_support::Workspace::new();
        let store = FileMetadataStore::new(&workspace.path);
        fs::write(workspace.path.join("versions.json"), b"legacy").unwrap();
        assert_eq!(
            store.read(crate::metadata::MANIFEST).unwrap().unwrap(),
            b"legacy"
        );
        assert!(store.read(URL).unwrap().is_none());
        let path = store.path(URL);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"TUI-METADATA-1\nhttps://other.invalid/catalog\n7").unwrap();
        assert!(store.read(URL).is_err());
    }
    #[test]
    fn file_store_round_trip_keeps_request_queries_distinct() {
        let workspace = crate::test_support::Workspace::new();
        let store = FileMetadataStore::new(&workspace.path);
        let first = "https://example.invalid/list?game=1.20.1";
        let second = "https://example.invalid/list?game=1.21.1";
        store.write(first, b"1").unwrap();
        store.write(second, b"2").unwrap();
        assert_eq!(store.read(first).unwrap().unwrap(), b"1");
        assert_eq!(store.read(second).unwrap().unwrap(), b"2");
        store.write(first, b"3").unwrap();
        assert_eq!(store.read(first).unwrap().unwrap(), b"3");
    }
}
