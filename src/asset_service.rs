use crate::{
    download::{valid_file, Download, Downloader},
    metadata::valid_id,
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{collections::HashMap, fs, io::Read, path::Path};

const INDEX_LIMIT: u64 = 64 * 1024 * 1024;
// Historical Mojang index packages, also identified by HMCL's manifest fallback.
const HISTORICAL_INDEXES: &[(&str, &str, u64)] = &[
    ("1.8", "f6ad102bcaa53b1a58358f16e376d548d44933ec", 78494),
    ("14w31a", "10a2a0e75b03cfb5a7196abbdf43b54f7fa61deb", 71214),
    ("14w25a", "32ff354a3be1c4dd83027111e6d79ee4d701d2c0", 69887),
    ("1.7.4", "545510a60f526b9aa8a38f9c0bc7a74235d21675", 59675),
    ("1.7.10", "1863782e33ce7b584fc45b037325a1964e095d3e", 72996),
    ("1.7.3", "f6cf726f4747128d13887010c2cbc44ba83504d9", 59227),
    ("pre-1.6", "3d8e55480977e32acd9844e545177e69a52f594b", 74091),
    ("legacy", "770572e819335b6c0a053f8378ad88eda189fc14", 109634),
];

#[derive(Clone, Copy)]
pub(crate) enum AssetCheck {
    Launch,
    Full,
}

/// Shared content-addressed assets; per-instance legacy mapping is a later step.
pub(crate) struct AssetRepository<'a> {
    root: &'a Path,
    object_base: &'a str,
}
impl<'a> AssetRepository<'a> {
    pub(crate) fn new(root: &'a Path) -> Self {
        Self {
            root,
            object_base: "https://resources.download.minecraft.net",
        }
    }

    pub(crate) fn version_index(
        &self,
        meta: &Value,
        instance_id: &str,
        downloader: &Downloader,
    ) -> Result<(String, Value)> {
        let description = Self::index_description(meta, instance_id)?;
        self.index(&description, instance_id, downloader)
    }
    fn index_description(meta: &Value, instance_id: &str) -> Result<Value> {
        fn optional_id<'v>(value: Option<&'v Value>, field: &str) -> Result<Option<&'v str>> {
            match value {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(id)) => {
                    valid_id(id).with_context(|| format!("资源索引字段 {field} 无效"))?;
                    Ok(Some(id))
                }
                _ => bail!("资源索引字段 {field} 必须为字符串"),
            }
        }
        if let Some(description) = meta.get("assetIndex").filter(|value| !value.is_null()) {
            if !description.is_object() {
                bail!("assetIndex 必须为对象");
            }
            let id = match optional_id(description.get("id"), "assetIndex.id")? {
                Some(id) => id,
                None => optional_id(meta.get("assets"), "assets")?.unwrap_or(instance_id),
            };
            valid_id(id)?;
            let mut description = description.clone();
            description["id"] = id.into();
            return Ok(description);
        }
        let id = optional_id(meta.get("assets"), "assets")?.unwrap_or("legacy");
        if let Some((_, hash, size)) = HISTORICAL_INDEXES.iter().find(|(known, _, _)| *known == id)
        {
            return Ok(serde_json::json!({
                "id":id, "sha1":hash, "size":size,
                "url":format!("https://launchermeta.mojang.com/v1/packages/{hash}/{id}.json")
            }));
        }
        // Keep custom offline indexes usable, but do not invent a URL or map
        // an unknown modern/custom asset set to unrelated legacy resources.
        Ok(serde_json::json!({"id":id}))
    }

    pub(crate) fn index(
        &self,
        description: &Value,
        fallback_id: &str,
        downloader: &Downloader,
    ) -> Result<(String, Value)> {
        downloader.control.check()?;
        let id = description["id"].as_str().unwrap_or(fallback_id);
        valid_id(id)?;
        let path = self.root.join("assets/indexes").join(format!("{id}.json"));
        let hash = hash_field(description.get("sha1"))?;
        let size = size_field(description.get("size"))?;
        if size.is_some_and(|size| size > INDEX_LIMIT) {
            bail!("资源索引超过 64 MiB 限制");
        }
        if !valid_file(&path, hash.as_deref(), size)? {
            let url = description["url"]
                .as_str()
                .filter(|url| !url.is_empty())
                .context("资源索引缺失或损坏，且版本没有提供索引下载地址，请修复版本配置")?;
            downloader.fetch(&Download {
                url: url.into(),
                dest: path.clone(),
                sha1: hash,
                size,
                alternatives: vec![],
            })?;
        }
        downloader.control.check()?;
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take(INDEX_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > INDEX_LIMIT {
            bail!("资源索引超过 64 MiB 限制");
        }
        downloader.control.check()?;
        let index: Value =
            serde_json::from_slice(&bytes).context("资源索引 JSON 损坏，请修复实例")?;
        Ok((id.into(), index))
    }

    fn objects(&self, index: &Value, control: &TaskControl) -> Result<Vec<Download>> {
        control.check()?;
        for key in ["virtual", "map_to_resources"] {
            if index
                .get(key)
                .is_some_and(|value| !value.is_null() && !value.is_boolean())
            {
                bail!("资源索引 {key} 必须为布尔值");
            }
        }
        let objects = index["objects"].as_object().context("资源索引无 objects")?;
        let mut positions = HashMap::new();
        let mut downloads: Vec<Download> = vec![];
        for (name, object) in objects {
            control.check()?;
            if index["virtual"].as_bool() == Some(true)
                || index["map_to_resources"].as_bool() == Some(true)
            {
                crate::metadata::safe_path(Path::new("resources"), name)?;
            }
            let hash = hash_field(object.get("hash"))?.context("资源缺少 hash")?;
            let size = size_field(object.get("size"))?;
            if let Some(&position) = positions.get(&hash) {
                let existing: &mut Download = &mut downloads[position];
                if matches!((existing.size, size), (Some(a), Some(b)) if a != b) {
                    bail!("同一资源 hash 对应不同大小：{hash}");
                }
                if existing.size.is_none() {
                    existing.size = size;
                }
                continue;
            }
            let relative = format!("{}/{hash}", &hash[..2]);
            positions.insert(hash.clone(), downloads.len());
            downloads.push(Download {
                url: format!("{}/{relative}", self.object_base),
                dest: self.root.join("assets/objects").join(relative),
                sha1: Some(hash),
                size,
                alternatives: vec![],
            });
        }
        Ok(downloads)
    }

    fn plan(
        &self,
        index: &Value,
        check: AssetCheck,
        control: &TaskControl,
    ) -> Result<Vec<Download>> {
        // Reject malformed/conflicting entries before downloading any objects.
        let mut downloads = self.objects(index, control)?;
        if matches!(check, AssetCheck::Launch) {
            let mut missing = vec![];
            for entry in downloads {
                control.check()?;
                // Fast launch scan intentionally does not hash every cached object.
                if !valid_file(&entry.dest, None, entry.size)? {
                    missing.push(entry);
                }
            }
            downloads = missing;
        }
        Ok(downloads)
    }

    pub(crate) fn prepare(
        &self,
        index: &Value,
        check: AssetCheck,
        downloader: &Downloader,
        threads: usize,
    ) -> Result<()> {
        downloader.many(
            self.plan(index, check, &downloader.control)?,
            threads,
            "游戏资源",
        )
    }
}

fn hash_field(value: Option<&Value>) -> Result<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let hash = value.as_str().context("资源 SHA-1 必须为字符串")?;
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("无效资源 SHA-1");
    }
    Ok(Some(hash.to_ascii_lowercase()))
}
fn size_field(value: Option<&Value>) -> Result<Option<u64>> {
    value
        .filter(|value| !value.is_null())
        .map(|value| value.as_u64().context("资源大小必须为非负整数"))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha1::{Digest, Sha1};
    const HASH: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";

    fn index() -> Value {
        json!({"objects":{"sound.ogg":{"hash":HASH,"size":3}}})
    }
    fn downloader() -> Downloader {
        let (tx, _rx) = std::sync::mpsc::channel();
        Downloader {
            retry: Default::default(),
            client: reqwest::blocking::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .unwrap(),
            control: TaskControl::default(),
            tx,
            source: "official".into(),
        }
    }
    #[test]
    fn historical_selection_uses_assets_and_preserves_modern_metadata_priority() {
        let description =
            AssetRepository::index_description(&json!({"assets":"pre-1.6"}), "renamed-instance")
                .unwrap();
        assert_eq!(description["id"], "pre-1.6");
        assert_eq!(
            description["sha1"],
            "3d8e55480977e32acd9844e545177e69a52f594b"
        );
        assert_eq!(
            AssetRepository::index_description(&json!({}), "renamed-instance").unwrap()["id"],
            "legacy"
        );
        let explicit = json!({"id":"modern","url":"https://example.invalid/index","sha1":HASH});
        assert_eq!(
            AssetRepository::index_description(
                &json!({"assetIndex":explicit,"assets":"legacy"}),
                "test"
            )
            .unwrap(),
            explicit
        );
        let with_assets = AssetRepository::index_description(
            &json!({"assetIndex":{"url":"https://example.invalid/index"},"assets":"custom"}),
            "renamed",
        )
        .unwrap();
        assert_eq!(with_assets["id"], "custom");
        for meta in [
            json!({"assets":"../bad"}),
            json!({"assetIndex":{"id":3}}),
            json!({"assetIndex":[]}),
        ] {
            assert!(AssetRepository::index_description(&meta, "test").is_err());
        }
    }
    #[test]
    fn custom_assets_without_modern_descriptor_reuse_only_the_named_offline_index() {
        let temp = crate::test_support::Workspace::new();
        fs::create_dir_all(temp.path.join("assets/indexes")).unwrap();
        fs::write(
            temp.path.join("assets/indexes/custom.json"),
            br#"{"objects":{}}"#,
        )
        .unwrap();
        let repository = AssetRepository::new(&temp.path);
        let downloader = downloader();
        let (id, _) = repository
            .version_index(
                &json!({"assets":"custom"}),
                "different-instance",
                &downloader,
            )
            .unwrap();
        assert_eq!(id, "custom");
        assert!(repository
            .version_index(
                &json!({"assets":"unknown-modern"}),
                "different-instance",
                &downloader
            )
            .is_err());
        assert!(!temp.path.join("assets/indexes/legacy.json").exists());
    }
    #[test]
    #[ignore = "requires the eight previously verified official indexes in .tools/asset-indexes; no network"]
    fn historical_official_cache_matches_descriptors_and_layout_flags() {
        let temp = crate::test_support::Workspace::new();
        let directory = temp.path.join("assets/indexes");
        fs::create_dir_all(&directory).unwrap();
        for (id, _, _) in HISTORICAL_INDEXES {
            fs::copy(
                Path::new(".tools/asset-indexes").join(format!("{id}.json")),
                directory.join(format!("{id}.json")),
            )
            .unwrap();
            let (selected, value) = AssetRepository::new(&temp.path)
                .version_index(&json!({"assets":id}), "renamed-instance", &downloader())
                .unwrap();
            assert_eq!(selected, *id);
            assert!(!value["objects"].as_object().unwrap().is_empty());
            if *id == "pre-1.6" {
                assert_eq!(value["map_to_resources"], true);
            }
            if *id == "legacy" {
                assert_eq!(value["virtual"], true);
            }
        }
    }

    #[test]
    fn fast_launch_repairs_missing_or_wrong_size_without_hashing_every_file() {
        let temp = crate::test_support::Workspace::new();
        let repository = AssetRepository::new(&temp.path);
        let control = TaskControl::default();
        let jobs = repository
            .plan(&index(), AssetCheck::Launch, &control)
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].sha1.as_deref(), Some(HASH));
        fs::create_dir_all(jobs[0].dest.parent().unwrap()).unwrap();
        fs::write(&jobs[0].dest, b"x").unwrap();
        assert_eq!(
            repository
                .plan(&index(), AssetCheck::Launch, &control)
                .unwrap()
                .len(),
            1
        );
        fs::write(&jobs[0].dest, b"bad").unwrap();
        assert!(repository
            .plan(&index(), AssetCheck::Launch, &control)
            .unwrap()
            .is_empty());
        let full = repository
            .plan(&index(), AssetCheck::Full, &control)
            .unwrap();
        assert_eq!(full.len(), 1);
        assert!(!crate::download::cached(&full[0]).unwrap());
        fs::write(&jobs[0].dest, b"abc").unwrap();
        assert!(crate::download::cached(&full[0]).unwrap());
        repository
            .prepare(&index(), AssetCheck::Full, &downloader(), 1)
            .unwrap();
        control.cancel();
        assert!(repository
            .plan(&index(), AssetCheck::Launch, &control)
            .is_err());
    }

    #[test]
    fn aliases_share_canonical_object_and_conflicting_metadata_is_rejected() {
        let temp = crate::test_support::Workspace::new();
        let repository = AssetRepository::new(&temp.path);
        let mut index = json!({"objects":{"first":{"hash":HASH.to_uppercase()},"second":{"hash":HASH,"size":3}}});
        let jobs = repository
            .plan(&index, AssetCheck::Full, &TaskControl::default())
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].size, Some(3));
        assert!(jobs[0]
            .dest
            .ends_with(Path::new("assets/objects/a9").join(HASH)));
        index["objects"]["first"]["size"] = json!(4);
        assert!(repository
            .plan(&index, AssetCheck::Full, &TaskControl::default())
            .is_err());
        assert!(!temp.path.join("assets").exists());
    }

    #[test]
    fn malformed_indices_fail_before_asset_writes() {
        let temp = crate::test_support::Workspace::new();
        let repository = AssetRepository::new(&temp.path);
        for index in [
            json!({}),
            json!({"objects":[]}),
            json!({"objects":{"a":{"hash":"../bad"}}}),
            json!({"objects":{"a":{"hash":HASH,"size":-1}}}),
            json!({"virtual":"true","objects":{}}),
            json!({"virtual":true,"objects":{"../bad":{"hash":HASH}}}),
        ] {
            assert!(repository
                .plan(&index, AssetCheck::Full, &TaskControl::default())
                .is_err());
        }
        assert!(!temp.path.join("assets").exists());
    }

    #[test]
    fn cached_index_and_objects_work_offline_and_invalid_index_metadata_fails() {
        let temp = crate::test_support::Workspace::new();
        let repository = AssetRepository::new(&temp.path);
        let downloader = downloader();
        let bytes = serde_json::to_vec(&json!({"objects":{}})).unwrap();
        fs::create_dir_all(temp.path.join("assets/indexes")).unwrap();
        fs::write(temp.path.join("assets/indexes/test.json"), &bytes).unwrap();
        let description =
            json!({"id":"test", "sha1":format!("{:x}", Sha1::digest(&bytes)), "size":bytes.len()});
        let (id, value) = repository
            .index(&description, "unused", &downloader)
            .unwrap();
        assert_eq!(id, "test");
        repository
            .prepare(&value, AssetCheck::Launch, &downloader, 1)
            .unwrap();
        assert!(repository
            .index(&json!({"id":"missing"}), "unused", &downloader)
            .is_err());
        assert!(repository
            .index(&json!({"id":"test", "sha1":"bad"}), "unused", &downloader)
            .is_err());
        assert!(repository
            .index(&json!({"size":INDEX_LIMIT+1}), "test", &downloader)
            .is_err());
        downloader.control.cancel();
        assert!(repository
            .index(&description, "unused", &downloader)
            .is_err());
    }

    #[test]
    fn local_source_installs_and_repairs_index_and_objects_then_reuses_offline_cache() {
        use std::{
            io::Write,
            net::TcpListener,
            thread,
            time::{Duration, Instant},
        };
        let temp = crate::test_support::Workspace::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let index_bytes = serde_json::to_vec(&index()).unwrap();
        let description = json!({"id":"test", "url":format!("{base}/index.json"), "sha1":format!("{:x}", Sha1::digest(&index_bytes)), "size":index_bytes.len()});
        let server_index = index_bytes.clone();
        let server = thread::spawn(move || {
            let mut paths = vec![];
            let deadline = Instant::now() + Duration::from_secs(5);
            while paths.len() < 4 && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                // On Windows an accepted stream can inherit the listener's
                // nonblocking mode; switch it back before the HTTP fixture read.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0; 4096];
                let bytes_read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..bytes_read]);
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                let body: &[u8] = if path == "/index.json" {
                    &server_index
                } else {
                    assert_eq!(path, format!("/a9/{HASH}"));
                    b"abc"
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
                paths.push(path);
            }
            paths
        });
        let repository = AssetRepository {
            root: &temp.path,
            object_base: &base,
        };
        let mut downloader = downloader();
        downloader.client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let (_, index) = repository
            .index(&description, "unused", &downloader)
            .unwrap();
        repository
            .prepare(&index, AssetCheck::Launch, &downloader, 2)
            .unwrap();
        let object_path = temp.path.join("assets/objects/a9").join(HASH);
        assert_eq!(fs::read(&object_path).unwrap(), b"abc");
        fs::write(
            temp.path.join("assets/indexes/test.json"),
            vec![b'x'; index_bytes.len()],
        )
        .unwrap();
        fs::write(&object_path, b"bad").unwrap();
        let (_, index) = repository
            .index(&description, "unused", &downloader)
            .unwrap();
        repository
            .prepare(&index, AssetCheck::Full, &downloader, 2)
            .unwrap();
        let paths = server.join().unwrap();
        assert_eq!(
            paths,
            [
                "/index.json".to_owned(),
                format!("/a9/{HASH}"),
                "/index.json".to_owned(),
                format!("/a9/{HASH}")
            ]
        );
        assert_eq!(fs::read(&object_path).unwrap(), b"abc");
        let (_, index) = repository
            .index(&description, "unused", &downloader)
            .unwrap();
        repository
            .prepare(&index, AssetCheck::Full, &downloader, 1)
            .unwrap();
    }
}
