use crate::{backend::Message, task::TaskControl};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::{
    blocking::Client,
    header::{CONTENT_RANGE, RANGE},
    StatusCode,
};
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::Sender,
        Mutex,
    },
    thread,
};

#[derive(Clone, Debug)]
pub struct Download {
    pub url: String,
    pub dest: PathBuf,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub alternatives: Vec<String>,
}
pub struct Downloader {
    pub client: Client,
    pub control: TaskControl,
    pub tx: Sender<Message>,
    pub source: String,
}
impl Downloader {
    pub fn fetch(&self, entry: &Download) -> Result<()> {
        let label = entry.dest.file_name().unwrap_or_default().to_string_lossy();
        let meter = crate::download_progress::Meter::new(&label, [entry.size], &self.tx);
        let result = self.fetch_tracked(entry, &meter, 0);
        meter.finish();
        result
    }
    fn fetch_tracked(
        &self,
        entry: &Download,
        meter: &crate::download_progress::Meter<'_>,
        job: usize,
    ) -> Result<()> {
        self.control.check()?;
        if cached(entry)? {
            let size = fs::metadata(&entry.dest)?.len();
            meter.update(job, size, Some(size), 0);
            return Ok(());
        }
        let parsed = reqwest::Url::parse(&entry.url)?;
        if parsed.scheme() != "https"
            && !(cfg!(test) && parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1"))
        {
            bail!("下载必须使用 HTTPS");
        }
        fs::create_dir_all(entry.dest.parent().context("下载目标缺少父目录")?)?;
        let mut partial_name = entry.dest.as_os_str().to_os_string();
        partial_name.push(".part");
        let partial = PathBuf::from(partial_name);
        let mut last = None;
        for attempt in 0..3 {
            self.control.check()?;
            let mut result = Err(anyhow!("没有可用下载地址"));
            let mut candidates = crate::sources::urls(&entry.url, &self.source);
            for alternative in &entry.alternatives {
                candidates.extend(crate::sources::urls(alternative, &self.source));
            }
            let mut used = std::collections::HashSet::new();
            candidates.retain(|url| used.insert(url.clone()));
            for url in candidates {
                let parsed = reqwest::Url::parse(&url)?;
                if parsed.scheme() != "https"
                    && !(cfg!(test)
                        && parsed.scheme() == "http"
                        && parsed.host_str() == Some("127.0.0.1"))
                {
                    continue;
                }
                self.control.check()?;
                let mut candidate = entry.clone();
                candidate.url = url;
                result = self.attempt(&candidate, &partial, meter, job);
                if result.is_ok() || self.control.cancelled() {
                    break;
                }
            }
            match result {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if self.control.cancelled() {
                        return Err(error);
                    }
                    let _ = self.tx.send(Message::Log(format!(
                        "重试 {}/3 · {} · {error}",
                        attempt + 1,
                        entry.dest.file_name().unwrap_or_default().to_string_lossy()
                    )));
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("下载失败")))
            .with_context(|| format!("下载 {}", entry.url))
    }
    fn attempt(
        &self,
        entry: &Download,
        partial: &Path,
        meter: &crate::download_progress::Meter<'_>,
        job: usize,
    ) -> Result<()> {
        let offset = fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
        let mut request = self.client.get(&entry.url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let mut response = request.send()?;
        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            // A complete .part can remain after a crash just before rename.
            if offset > 0
                && (entry.sha1.is_some() || entry.size.is_some())
                && valid_file(partial, entry.sha1.as_deref(), entry.size)?
                && valid_archive(partial, &entry.dest)?
            {
                meter.update(job, offset, Some(offset), 0);
                return commit(partial, &entry.dest);
            }
            let _ = fs::remove_file(partial);
            bail!("服务器拒绝断点，下一次从头下载");
        }
        response.error_for_status_ref()?;
        let resume = offset > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
        let mut range_total = None;
        if response.status() == StatusCode::PARTIAL_CONTENT {
            let range = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|s| s.to_str().ok())
                .context("断点响应缺少 Content-Range")?;
            let (end, total) = parse_content_range(range, if resume { offset } else { 0 })?;
            if total.is_none() && entry.size.is_none() && entry.sha1.is_none() {
                bail!("断点响应未提供总大小，且没有文件校验信息，无法确认下载完整");
            }
            if entry
                .size
                .is_some_and(|size| total.is_some_and(|total| total != size))
            {
                bail!("断点响应的总大小与下载元数据不符");
            }
            if response
                .content_length()
                .is_some_and(|length| length != end - if resume { offset } else { 0 } + 1)
            {
                bail!("断点响应的长度与 Content-Range 不符");
            }
            range_total = total;
        }
        let expected_len = range_total.or_else(|| {
            response
                .content_length()
                .map(|len| len + if resume { offset } else { 0 })
        });
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resume)
            .truncate(!resume)
            .open(partial)?;
        let mut buffer = [0u8; 65536];
        let mut position = if resume { offset } else { 0 };
        meter.update(job, position, entry.size.or(expected_len), 0);
        loop {
            self.control.check()?;
            let n = response.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n])?;
            position += n as u64;
            meter.update(job, position, entry.size.or(expected_len), n as u64);
        }
        file.sync_all()?;
        drop(file);
        if expected_len.is_some_and(|size| {
            fs::metadata(partial)
                .map(|m| m.len() != size)
                .unwrap_or(true)
        }) {
            bail!("文件尚未下载完整");
        }
        if !valid_file(partial, entry.sha1.as_deref(), entry.size)? {
            fs::remove_file(partial)?;
            bail!("文件大小或 SHA-1 校验失败");
        }
        if !valid_archive(partial, &entry.dest)? {
            fs::remove_file(partial)?;
            bail!("下载结果不是有效 JAR / ZIP 文件");
        }
        self.control.check()?;
        meter.update(job, position, Some(position), 0);
        commit(partial, &entry.dest)
    }
    pub fn many(&self, entries: Vec<Download>, workers: usize, label: &str) -> Result<()> {
        let mut seen = HashMap::new();
        let mut jobs = Vec::new();
        for entry in entries {
            if let Some(url) = seen.insert(entry.dest.clone(), entry.url.clone()) {
                if url != entry.url {
                    bail!("不同下载指向相同文件：{}", entry.dest.display());
                }
                continue;
            }
            jobs.push(entry);
        }
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let failure = Mutex::new(None);
        let total = jobs.len();
        let meter =
            crate::download_progress::Meter::new(label, jobs.iter().map(|job| job.size), &self.tx);
        let _ = self.tx.send(Message::Progress(label.into(), 0, total));
        thread::scope(|scope| {
            for _ in 0..workers.clamp(1, 16).min(total) {
                scope.spawn(|| loop {
                    if failure.lock().unwrap().is_some() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else {
                        break;
                    };
                    if let Err(error) = self.fetch_tracked(job, &meter, i) {
                        let mut slot = failure.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                        break;
                    }
                    let count = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = self.tx.send(Message::Progress(label.into(), count, total));
                });
            }
        });
        meter.finish();
        if let Some(error) = failure.into_inner().unwrap() {
            return Err(error);
        }
        self.control.check()
    }
}
fn valid_archive(path: &Path, destination: &Path) -> Result<bool> {
    Ok(!is_archive(destination) || zip::ZipArchive::new(fs::File::open(path)?).is_ok())
}
fn is_archive(destination: &Path) -> bool {
    destination
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar") || ext.eq_ignore_ascii_case("zip"))
}

fn parse_content_range(header: &str, offset: u64) -> Result<(u64, Option<u64>)> {
    let range = header.strip_prefix("bytes ").context("断点响应单位无效")?;
    let (bounds, total) = range.split_once('/').context("断点响应缺少总大小")?;
    let (start, end) = bounds.split_once('-').context("断点响应范围无效")?;
    let start = start.parse::<u64>().context("断点起始位置无效")?;
    let end = end.parse::<u64>().context("断点结束位置无效")?;
    let total = if total == "*" {
        None
    } else {
        Some(total.parse::<u64>().context("断点总大小无效")?)
    };
    if start != offset || end < start || end == u64::MAX || total.is_some_and(|n| end >= n) {
        bail!("断点响应范围与请求不符");
    }
    Ok((end, total))
}
fn commit(partial: &Path, dest: &Path) -> Result<()> {
    fs::rename(partial, dest)?;
    Ok(())
}
fn cached(entry: &Download) -> Result<bool> {
    if !entry.dest.is_file() {
        return Ok(false);
    }
    if !valid_file(&entry.dest, entry.sha1.as_deref(), entry.size)? {
        return Ok(false);
    }
    if entry.sha1.is_some() || entry.size.is_some() {
        return valid_archive(&entry.dest, &entry.dest);
    }
    // Maven profiles may omit hashes: a parseable archive avoids re-downloading every repair.
    if is_archive(&entry.dest) {
        return valid_archive(&entry.dest, &entry.dest);
    }
    Ok(false)
}
pub fn valid_file(path: &Path, expected: Option<&str>, size: Option<u64>) -> Result<bool> {
    if size.is_some_and(|size| fs::metadata(path).map(|m| m.len() != size).unwrap_or(true)) {
        return Ok(false);
    }
    let Some(expected) = expected else {
        return Ok(true);
    };
    let mut file = fs::File::open(path)?;
    let mut hash = Sha1::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    #[test]
    fn validates_complete_range_instead_of_only_start_prefix() {
        assert_eq!(parse_content_range("bytes 1-2/3", 1).unwrap(), (2, Some(3)));
        assert_eq!(parse_content_range("bytes 1-2/*", 1).unwrap(), (2, None));
        for invalid in [
            "bytes 1-0/3",
            "bytes 1-3/3",
            "bytes 1-2/bad",
            "bytes 10-12/13",
            "bytes 1-18446744073709551615/*",
            "items 1-2/3",
        ] {
            assert!(parse_content_range(invalid, 1).is_err(), "{invalid}");
        }
    }
    #[test]
    fn uppercase_archives_are_validated() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("download.part");
        fs::write(&path, b"not a jar").unwrap();
        assert!(!valid_archive(&path, Path::new("mod.JAR")).unwrap());
        assert!(!valid_archive(&path, Path::new("pack.ZIP")).unwrap());
        assert!(valid_archive(&path, Path::new("asset")).unwrap());
    }
    fn request_test(
        name: &str,
        partial_bytes: &[u8],
        response: &'static str,
        expected_header: &'static str,
    ) {
        let root = std::env::temp_dir().join(format!("tui-download-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("file");
        let partial = root.join("file.part");
        fs::write(&partial, partial_bytes).unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let mut bytes = vec![];
            let mut byte = [0];
            while socket.read(&mut byte).unwrap() > 0 {
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap().to_lowercase();
            assert!(request.contains(expected_header));
            socket.write_all(response.as_bytes()).unwrap();
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let downloader = Downloader {
            client: Client::new(),
            control: TaskControl::default(),
            tx,
            source: "official".into(),
        };
        downloader
            .fetch(&Download {
                url: format!("http://{address}/file"),
                dest: dest.clone(),
                sha1: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
                size: Some(3),
                alternatives: vec![],
            })
            .unwrap();
        handle.join().unwrap();
        let progress = rx
            .try_iter()
            .filter_map(|message| match message {
                Message::Transfer(snapshot) => Some(snapshot),
                _ => None,
            })
            .last()
            .unwrap();
        assert_eq!(progress.done, 3);
        assert_eq!(progress.total, Some(3));
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
        assert!(!partial.exists());
        fs::remove_file(dest).unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn resumes_range_response() {
        request_test("resume", b"a", "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 1-2/3\r\nConnection: close\r\n\r\nbc", "range: bytes=1-");
    }
    #[test]
    fn complete_partial_is_committed_after_range_rejection() {
        request_test(
            "complete",
            b"abc",
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "range: bytes=3-",
        );
    }
    #[test]
    fn resets_when_server_ignores_range() {
        request_test(
            "restart",
            b"garbage",
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
            "range: bytes=7-",
        );
    }
    #[test]
    fn cancelled_task_does_not_touch_destination() {
        let (tx, _) = std::sync::mpsc::channel();
        let control = TaskControl::default();
        control.cancel();
        let downloader = Downloader {
            client: Client::new(),
            control,
            tx,
            source: "official".into(),
        };
        assert!(downloader
            .fetch(&Download {
                url: "https://example.com".into(),
                dest: PathBuf::from("must-not-exist"),
                sha1: None,
                size: None,
                alternatives: vec![],
            })
            .is_err());
        assert!(!Path::new("must-not-exist").exists());
    }
}
