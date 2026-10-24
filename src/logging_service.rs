use crate::download::{Download, Downloader};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;

/// One metadata logging configuration, shared by repair and launch preparation.
pub(crate) struct LoggingConfiguration {
    download: Download,
    argument: String,
}

impl LoggingConfiguration {
    pub(crate) fn from_metadata(root: &Path, metadata: &Value) -> Result<Option<Self>> {
        let Some(logging) = metadata
            .pointer("/logging/client")
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let file = &logging["file"];
        let name = file["id"].as_str().context("日志配置缺少文件 ID")?;
        crate::metadata::valid_id(name)?;
        let argument = logging["argument"]
            .as_str()
            .filter(|value| !value.is_empty())
            .context("日志配置缺少 JVM 参数")?;
        if argument.contains('\0') {
            bail!("日志 JVM 参数包含 NUL");
        }
        let sha1 = match file.get("sha1").filter(|value| !value.is_null()) {
            Some(value) => {
                let hash = value.as_str().context("日志配置 SHA-1 格式错误")?;
                if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    bail!("日志配置 SHA-1 格式错误");
                }
                Some(hash.to_owned())
            }
            None => None,
        };
        let size = match file.get("size").filter(|value| !value.is_null()) {
            Some(value) => Some(value.as_u64().context("日志配置大小必须为非负整数")?),
            None => None,
        };
        Ok(Some(Self {
            download: Download {
                url: file["url"]
                    .as_str()
                    .filter(|url| !url.is_empty())
                    .context("日志配置缺少下载地址")?
                    .into(),
                dest: crate::metadata::safe_path(&root.join("assets/log_configs"), name)?,
                sha1,
                size,
                alternatives: vec![],
            },
            // Expand the actual path with the other original templates later.
            // A literal ${...} inside the path must not be expanded a second time.
            argument: argument.replace("${path}", "${logging_config_path}"),
        }))
    }

    pub(crate) fn prepare(&self, downloader: &Downloader) -> Result<()> {
        downloader
            .fetch(&self.download)
            .context("准备游戏日志配置失败")
    }

    pub(crate) fn path(&self) -> &Path {
        &self.download.dest
    }
    pub(crate) fn argument(&self) -> &str {
        &self.argument
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha1::{Digest, Sha1};

    fn metadata(bytes: &[u8]) -> Value {
        json!({"logging":{"client":{"argument":"-Dlog4j.configurationFile=${path}", "file": {
            "id":"client.xml", "url":"https://example.invalid/client.xml",
            "sha1":format!("{:x}", Sha1::digest(bytes)), "size":bytes.len()
        }}}})
    }

    #[test]
    fn missing_logging_is_optional_but_invalid_metadata_is_not_silently_ignored() {
        for meta in [
            json!({}),
            json!({"logging":{}}),
            json!({"logging":{"client":null}}),
        ] {
            assert!(
                LoggingConfiguration::from_metadata(Path::new("game"), &meta)
                    .unwrap()
                    .is_none()
            );
        }
        for (pointer, value) in [
            ("/logging/client/file/id", json!("../escape.xml")),
            ("/logging/client/file/id", json!("nested/client.xml")),
            ("/logging/client/file/sha1", json!("broken")),
            ("/logging/client/file/size", json!(-1)),
            ("/logging/client/argument", json!(null)),
            ("/logging/client/argument", json!("bad\0arg")),
        ] {
            let mut meta = metadata(b"xml");
            *meta.pointer_mut(pointer).unwrap() = value;
            assert!(
                LoggingConfiguration::from_metadata(Path::new("game"), &meta).is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn logging_paths_with_spaces_and_placeholder_text_remain_literal() {
        let configuration = LoggingConfiguration::from_metadata(
            Path::new("game ${profile} 中文 space"),
            &metadata(b"xml"),
        )
        .unwrap()
        .unwrap();
        let values = std::collections::HashMap::from([(
            "logging_config_path",
            configuration.path().display().to_string(),
        )]);
        let argument =
            crate::launch_options::expand_argument(configuration.argument(), &values).unwrap();
        assert_eq!(
            argument,
            format!(
                "-Dlog4j.configurationFile={}",
                configuration.path().display()
            )
        );
        assert!(argument.contains("${profile}"));
    }

    #[test]
    fn validated_logging_cache_is_offline_and_cancellation_is_respected() {
        let workspace = crate::test_support::Workspace::new();
        let bytes = b"<Configuration/>";
        let configuration = LoggingConfiguration::from_metadata(&workspace.path, &metadata(bytes))
            .unwrap()
            .unwrap();
        std::fs::create_dir_all(configuration.path().parent().unwrap()).unwrap();
        std::fs::write(configuration.path(), bytes).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let downloader = Downloader {
            retry: Default::default(),
            client: reqwest::blocking::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .unwrap(),
            control: crate::task::TaskControl::default(),
            tx,
            source: "official".into(),
        };
        configuration.prepare(&downloader).unwrap();
        downloader.control.cancel();
        assert!(configuration.prepare(&downloader).is_err());
        assert_eq!(std::fs::read(configuration.path()).unwrap(), bytes);
    }

    #[test]
    fn downloads_missing_logging_and_repairs_same_size_corruption() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
            time::Duration,
        };
        let workspace = crate::test_support::Workspace::new();
        let bytes = b"<Configuration/>";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/client.xml", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut count = 0;
            while count < 2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0; 4096];
                let bytes_read = stream.read(&mut request).unwrap();
                assert!(request[..bytes_read].starts_with(b"GET /client.xml "));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )
                .unwrap();
                stream.write_all(bytes).unwrap();
                count += 1;
            }
            count
        });
        let mut meta = metadata(bytes);
        meta["logging"]["client"]["file"]["url"] = json!(url);
        let configuration = LoggingConfiguration::from_metadata(&workspace.path, &meta)
            .unwrap()
            .unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let downloader = Downloader {
            retry: Default::default(),
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            control: crate::task::TaskControl::default(),
            tx,
            source: "official".into(),
        };
        configuration.prepare(&downloader).unwrap();
        assert_eq!(std::fs::read(configuration.path()).unwrap(), bytes);
        std::fs::write(configuration.path(), vec![b'x'; bytes.len()]).unwrap();
        let repaired = configuration.prepare(&downloader);
        assert_eq!(server.join().unwrap(), 2);
        repaired.unwrap();
        assert_eq!(std::fs::read(configuration.path()).unwrap(), bytes);
        // Server has closed: a fully verified cache must not contact it again.
        configuration.prepare(&downloader).unwrap();
    }
}
