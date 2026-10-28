use crate::{backend::Message, task::TaskControl};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::{
    blocking::Client,
    header::{CONTENT_RANGE, DATE, ETAG, IF_RANGE, RANGE, RETRY_AFTER},
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
impl Download {
    fn validate_integrity(&self) -> Result<()> {
        if self.sha1.as_ref().is_some_and(|hash| {
            hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            bail!(
                "下载 SHA-1 必须为 40 位十六进制字符串：{}",
                self.dest.display()
            );
        }
        Ok(())
    }
}
/// A validated batch: each destination has one owner and one set of integrity constraints.
struct DownloadPlan {
    jobs: Vec<Download>,
}
impl DownloadPlan {
    fn new(entries: Vec<Download>) -> Result<Self> {
        let mut destinations: HashMap<PathBuf, usize> = HashMap::new();
        let mut jobs: Vec<Download> = Vec::new();
        for entry in entries {
            entry.validate_integrity()?;
            if let Some(&index) = destinations.get(&entry.dest) {
                let existing = &mut jobs[index];
                let same_hash = match (&existing.sha1, &entry.sha1) {
                    (Some(a), Some(b)) => {
                        if !a.eq_ignore_ascii_case(b) {
                            bail!("相同下载路径的 SHA-1 冲突：{}", entry.dest.display());
                        }
                        true
                    }
                    _ => false,
                };
                if matches!((existing.size, entry.size), (Some(a), Some(b)) if a != b) {
                    bail!("相同下载路径的文件大小冲突：{}", entry.dest.display());
                }
                if existing.url != entry.url && !same_hash {
                    bail!(
                        "不同下载指向相同文件且无法确认内容一致：{}",
                        entry.dest.display()
                    );
                }
                if existing.sha1.is_none() {
                    existing.sha1 = entry.sha1;
                }
                if existing.size.is_none() {
                    existing.size = entry.size;
                }
                for url in std::iter::once(entry.url).chain(entry.alternatives) {
                    if url.is_empty() {
                        continue;
                    }
                    if existing.url.is_empty() {
                        existing.url = url;
                        continue;
                    }
                    if url != existing.url && !existing.alternatives.contains(&url) {
                        existing.alternatives.push(url);
                    }
                }
            } else {
                destinations.insert(entry.dest.clone(), jobs.len());
                jobs.push(entry);
            }
        }
        Ok(Self { jobs })
    }
    fn jobs(&self) -> &[Download] {
        &self.jobs
    }
}
pub struct Downloader {
    pub(crate) retry: std::sync::Arc<crate::download_retry_service::DownloadRetryCoordinator>,
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
        entry.validate_integrity()?;
        if cached_with_control(entry, &self.control)? {
            let size = fs::metadata(&entry.dest)?.len();
            meter.update(job, size, Some(size), 0);
            return Ok(());
        }
        if entry.url.is_empty() {
            bail!(
                "安装器生成的本地依赖缺失或校验失败，请修复或重新安装实例：{}",
                entry.dest.display()
            );
        }
        let parsed = reqwest::Url::parse(&entry.url)?;
        if parsed.scheme() != "https"
            && !(cfg!(test) && parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1"))
        {
            bail!("下载必须使用 HTTPS");
        }
        let _lease =
            crate::download_lock::DownloadLease::acquire(&entry.dest, &self.control, || {
                let _ = self.tx.send(Message::Log(format!(
                    "等待其他任务完成共享下载：{}",
                    entry.dest.file_name().unwrap_or_default().to_string_lossy()
                )));
            })?;
        // A different process may have committed the file while we waited.
        if cached_with_control(entry, &self.control)? {
            let size = fs::metadata(&entry.dest)?.len();
            meter.update(job, size, Some(size), 0);
            return Ok(());
        }
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
            while let Some(next) =
                (0..candidates.len()).min_by_key(|index| self.retry.remaining(&candidates[*index]))
            {
                let url = candidates.remove(next);
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
                    if attempt < 2 {
                        self.control
                            .wait(std::time::Duration::from_millis(250 * (attempt + 1)))?;
                    }
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
        self.retry
            .wait(&entry.url, &self.control, |origin, seconds| {
                let _ = self.tx.send(Message::Log(format!(
                    "下载源暂时限流或不可用，等待约 {seconds} 秒：{origin}；可取消任务"
                )));
            })?;
        let binding = crate::download_resume_service::ResumeBinding::new(partial);
        let validator = entry
            .sha1
            .is_none()
            .then(|| binding.validator(&entry.url))
            .flatten();
        let offset = if entry.sha1.is_some() || validator.is_some() {
            fs::metadata(partial).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let mut request = self.client.get(&entry.url);
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
            if entry.sha1.is_none() {
                request = request.header(IF_RANGE, validator.as_deref().unwrap());
            }
        }
        let mut response = request.send()?;
        self.retry.response(
            &entry.url,
            response.status().as_u16(),
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            response
                .headers()
                .get(DATE)
                .and_then(|value| value.to_str().ok()),
        )?;
        if response.url().as_str() != entry.url {
            self.retry.response(
                response.url().as_str(),
                response.status().as_u16(),
                response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()),
                response
                    .headers()
                    .get(DATE)
                    .and_then(|value| value.to_str().ok()),
            )?;
        }
        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            // A complete .part can remain after a crash just before rename.
            if offset > 0
                && entry.sha1.is_some()
                && valid_file_with_control(
                    partial,
                    entry.sha1.as_deref(),
                    entry.size,
                    &self.control,
                )?
                && valid_archive(partial, &entry.dest)?
            {
                meter.update(job, offset, Some(offset), 0);
                self.control.check()?;
                commit(partial, &entry.dest)?;
                binding.clear();
                return Ok(());
            }
            let _ = fs::remove_file(partial);
            binding.clear();
            bail!("服务器拒绝断点，下一次从头下载");
        }
        response.error_for_status_ref()?;
        if !matches!(
            response.status(),
            StatusCode::OK | StatusCode::PARTIAL_CONTENT
        ) {
            bail!(
                "下载响应没有提供完整或部分文件（HTTP {}）",
                response.status().as_u16()
            );
        }
        let resume = offset > 0 && response.status() == StatusCode::PARTIAL_CONTENT;
        let response_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok());
        if resume && entry.sha1.is_none() && response_etag != validator.as_deref() {
            binding.clear();
            fs::remove_file(partial)?;
            bail!("断点响应的文件标识不一致，下一次从头下载");
        }
        let mut range_total = None;
        let mut body_length = response.content_length();
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
            if response
                .content_length()
                .is_some_and(|length| length != end - if resume { offset } else { 0 } + 1)
            {
                bail!("断点响应的长度与 Content-Range 不符");
            }
            body_length = Some(end - if resume { offset } else { 0 } + 1);
            range_total = total;
        }
        let mut extent = crate::transfer_extent_service::TransferExtent::new(
            if resume { offset } else { 0 },
            body_length,
            range_total,
            entry.size,
            response.status() == StatusCode::PARTIAL_CONTENT,
        )?;
        let expected_len = extent.total();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resume)
            .truncate(!resume)
            .open(partial)?;
        if entry.sha1.is_none() {
            // Truncate old bytes before publishing new validator metadata.
            binding.record(&entry.url, response_etag)?;
        } else {
            binding.invalidate()?;
        }
        let mut buffer = [0u8; 65536];
        let mut position = if resume { offset } else { 0 };
        meter.update(job, position, entry.size.or(expected_len), 0);
        loop {
            self.control.check()?;
            let n = response.read(&mut buffer)?;
            self.control.check()?;
            if n == 0 {
                break;
            }
            position = extent.accept(n)?;
            file.write_all(&buffer[..n])?;
            meter.update(job, position, entry.size.or(expected_len), n as u64);
        }
        extent.finish()?;
        file.sync_all()?;
        drop(file);
        if expected_len.is_some_and(|size| {
            fs::metadata(partial)
                .map(|m| m.len() != size)
                .unwrap_or(true)
        }) {
            bail!("文件尚未下载完整");
        }
        if !valid_file_with_control(partial, entry.sha1.as_deref(), entry.size, &self.control)? {
            fs::remove_file(partial)?;
            binding.clear();
            bail!("文件大小或 SHA-1 校验失败");
        }
        if !valid_archive(partial, &entry.dest)? {
            fs::remove_file(partial)?;
            binding.clear();
            bail!("下载结果不是有效 JAR / ZIP 文件");
        }
        self.control.check()?;
        meter.update(job, position, Some(position), 0);
        commit(partial, &entry.dest)?;
        binding.clear();
        Ok(())
    }
    pub fn many(&self, entries: Vec<Download>, workers: usize, label: &str) -> Result<()> {
        let plan = DownloadPlan::new(entries)?;
        let jobs = plan.jobs();
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
pub(crate) fn cached(entry: &Download) -> Result<bool> {
    cached_with_control(entry, &TaskControl::default())
}
fn cached_with_control(entry: &Download, control: &TaskControl) -> Result<bool> {
    control.check()?;
    if !entry.dest.is_file() {
        return Ok(false);
    }
    if !valid_file_with_control(&entry.dest, entry.sha1.as_deref(), entry.size, control)? {
        return Ok(false);
    }
    // Maven profiles may omit hashes: a parseable archive avoids re-downloading every repair.
    if entry.sha1.is_some() || entry.size.is_some() || is_archive(&entry.dest) {
        let valid = valid_archive(&entry.dest, &entry.dest)?;
        control.check()?;
        return Ok(valid);
    }
    Ok(false)
}
pub fn valid_file(path: &Path, expected: Option<&str>, size: Option<u64>) -> Result<bool> {
    valid_file_with_control(path, expected, size, &TaskControl::default())
}
pub(crate) fn valid_file_with_control(
    path: &Path,
    expected: Option<&str>,
    size: Option<u64>,
    control: &TaskControl,
) -> Result<bool> {
    control.check()?;
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法读取文件校验信息"),
    };
    if !metadata.is_file() || size.is_some_and(|size| metadata.len() != size) {
        return Ok(false);
    }
    let Some(expected) = expected else {
        return Ok(true);
    };
    let mut file = fs::File::open(path)?;
    hash_matches(&mut file, expected, control)
}
fn hash_matches(reader: &mut impl Read, expected: &str, control: &TaskControl) -> Result<bool> {
    let mut hash = Sha1::new();
    let mut buf = [0u8; 65536];
    loop {
        control.check()?;
        let n = reader.read(&mut buf)?;
        control.check()?;
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
    #[test]
    fn cancellation_during_hash_read_is_not_a_checksum_mismatch() {
        struct CancellingReader {
            control: TaskControl,
            reads: usize,
        }
        impl Read for CancellingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                assert_eq!(
                    self.reads, 1,
                    "must not read another block after cancellation"
                );
                buffer[..3].copy_from_slice(b"abc");
                self.control.cancel();
                Ok(3)
            }
        }
        let control = TaskControl::default();
        let mut reader = CancellingReader {
            control: control.clone(),
            reads: 0,
        };
        let error = hash_matches(
            &mut reader,
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            &control,
        )
        .unwrap_err();
        assert!(error.to_string().contains("取消"));
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("retained.part");
        fs::write(&path, b"abc").unwrap();
        assert!(valid_file_with_control(&path, Some("invalid"), None, &control).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"abc");
    }
    #[test]
    fn file_validation_requires_a_real_file_even_without_hash_or_size() {
        let workspace = crate::test_support::Workspace::new();
        let missing = workspace.path.join("missing.bin");
        assert!(!valid_file(&missing, None, None).unwrap());
        assert!(!valid_file(&missing, Some(&"a".repeat(40)), None).unwrap());
        assert!(!valid_file(&workspace.path, None, None).unwrap());
        let file = workspace.path.join("file.bin");
        fs::write(&file, b"abc").unwrap();
        assert!(valid_file(&file, None, None).unwrap());
        assert!(!valid_file(&file, None, Some(4)).unwrap());
        assert!(valid_file(
            &file,
            Some("A9993E364706816ABA3E25717850C26C9CD0D89D"),
            Some(3)
        )
        .unwrap());
    }
    use std::net::TcpListener;
    fn planned_entry(hash: Option<&str>, size: Option<u64>) -> Download {
        Download {
            url: "https://example.com/library.jar".into(),
            dest: "libraries/library.jar".into(),
            sha1: hash.map(str::to_owned),
            size,
            alternatives: vec![],
        }
    }
    #[test]
    fn waiting_download_reuses_the_other_writers_verified_result() {
        let workspace = crate::test_support::Workspace::new();
        let dest = workspace.path.join("shared.bin");
        let held =
            crate::download_lock::DownloadLease::acquire(&dest, &TaskControl::default(), || {})
                .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let entry = Download {
            url: "https://must-not-be-requested.invalid/shared".into(),
            dest: dest.clone(),
            sha1: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
            size: Some(3),
            alternatives: vec![],
        };
        let handle = thread::spawn(move || {
            let downloader = Downloader {
                retry: Default::default(),
                client: Client::builder()
                    .timeout(std::time::Duration::from_millis(100))
                    .build()
                    .unwrap(),
                control: TaskControl::default(),
                tx,
                source: "official".into(),
            };
            downloader.fetch(&entry)
        });
        assert!(
            matches!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), Message::Log(line) if line.contains("等待其他任务"))
        );
        fs::write(&dest, b"abc").unwrap();
        drop(held);
        handle.join().unwrap().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"abc");
        assert!(!workspace.path.join("shared.bin.part").exists());
    }
    #[test]
    fn download_plan_merges_stronger_constraints_and_identical_content_mirrors() {
        let mut mirror = planned_entry(Some("A9993E364706816ABA3E25717850C26C9CD0D89D"), Some(10));
        mirror.url = "https://mirror.example.com/library.jar".into();
        let plan = DownloadPlan::new(vec![
            planned_entry(None, None),
            planned_entry(Some("a9993e364706816aba3e25717850c26c9cd0d89d"), Some(10)),
            mirror,
        ])
        .unwrap();
        assert_eq!(plan.jobs().len(), 1);
        assert_eq!(
            plan.jobs()[0].sha1.as_deref(),
            Some("a9993e364706816aba3e25717850c26c9cd0d89d")
        );
        assert_eq!(plan.jobs()[0].size, Some(10));
        assert_eq!(plan.jobs()[0].alternatives.len(), 1);
    }
    #[test]
    fn local_and_remote_duplicate_merge_is_order_independent_with_matching_hash() {
        let mut local = planned_entry(Some("a9993e364706816aba3e25717850c26c9cd0d89d"), Some(3));
        local.url.clear();
        let mut remote = local.clone();
        remote.url = "https://example.com/generated.jar".into();
        for entries in [
            vec![local.clone(), remote.clone()],
            vec![remote.clone(), local.clone()],
        ] {
            let plan = DownloadPlan::new(entries).unwrap();
            assert_eq!(plan.jobs().len(), 1);
            assert_eq!(plan.jobs()[0].url, remote.url);
            assert!(plan.jobs()[0].alternatives.is_empty());
            assert_eq!(plan.jobs()[0].sha1, local.sha1);
        }
        local.sha1 = None;
        assert!(DownloadPlan::new(vec![local, remote]).is_err());
    }
    #[test]
    fn local_only_artifact_is_verified_without_a_download_url() {
        let workspace = crate::test_support::Workspace::new();
        let (tx, _rx) = std::sync::mpsc::channel();
        let downloader = Downloader {
            retry: Default::default(),
            client: Client::new(),
            control: TaskControl::default(),
            tx,
            source: "official".into(),
        };
        let entry = Download {
            url: String::new(),
            dest: workspace.path.join("generated.bin"),
            sha1: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
            size: Some(3),
            alternatives: vec![],
        };
        for bytes in [None, Some(b"bad".as_slice()), Some(b"abc".as_slice())] {
            if let Some(bytes) = bytes {
                fs::write(&entry.dest, bytes).unwrap();
            }
            let result = downloader.fetch(&entry);
            if bytes == Some(b"abc".as_slice()) {
                result.unwrap();
            } else {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("本地依赖缺失或校验失败"));
            }
        }
    }
    #[test]
    fn malformed_hash_is_rejected_before_url_parsing_or_destination_creation() {
        let workspace = crate::test_support::Workspace::new();
        let (tx, _rx) = std::sync::mpsc::channel();
        let downloader = Downloader {
            retry: Default::default(),
            client: Client::new(),
            control: TaskControl::default(),
            tx,
            source: "official".into(),
        };
        for hash in [
            "".to_owned(),
            "a".repeat(39),
            "a".repeat(41),
            "g".repeat(40),
        ] {
            let mut entry = planned_entry(Some(&hash), None);
            entry.url = "not a URL".into();
            entry.dest = workspace.path.join("uncreated/file.jar");
            let error = downloader.fetch(&entry).unwrap_err();
            assert!(error.to_string().contains("40 位十六进制"));
            assert!(DownloadPlan::new(vec![planned_entry(None, None), entry]).is_err());
            assert!(!workspace.path.join("uncreated").exists());
        }
    }
    #[test]
    fn conflicting_downloads_fail_before_any_network_or_file_writes() {
        for pair in [
            vec![
                planned_entry(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), None),
                planned_entry(Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"), None),
            ],
            vec![planned_entry(None, Some(1)), planned_entry(None, Some(2))],
        ] {
            assert!(DownloadPlan::new(pair).is_err());
        }
        let mut other = planned_entry(None, None);
        other.url = "https://example.com/different.jar".into();
        assert!(DownloadPlan::new(vec![planned_entry(None, None), other]).is_err());
    }
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
        let workspace = crate::test_support::Workspace::new();
        let root = workspace.path.join(name);
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("file");
        let partial = root.join("file.part");
        fs::write(&partial, partial_bytes).unwrap();
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut socket = crate::test_support::accept_http(&server).unwrap();
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
            retry: Default::default(),
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
    }
    #[test]
    fn resumes_range_response() {
        request_test("resume", b"a", "HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 1-2/3\r\nConnection: close\r\n\r\nbc", "range: bytes=1-");
    }
    #[test]
    fn rate_limited_origin_does_not_delay_an_available_mirror() {
        fn serve(response: impl Into<String>) -> (String, thread::JoinHandle<()>) {
            let response = response.into();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let handle = thread::spawn(move || {
                let mut socket = crate::test_support::accept_http(&listener).unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = vec![];
                let mut byte = [0];
                while socket.read(&mut byte).unwrap() > 0 {
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                socket.write_all(response.as_bytes()).unwrap();
            });
            (base, handle)
        }
        let workspace = crate::test_support::Workspace::new();
        let (limited, first) = serve("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let (redirect, redirected) = serve(format!("HTTP/1.1 302 Found\r\nLocation: {limited}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
        let (available, second) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc");
        let control = TaskControl::default();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = done.clone();
        let signal = control.clone();
        let timer = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !completed.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(10));
            }
            if !completed.load(Ordering::Relaxed) {
                signal.cancel();
            }
        });
        let (tx, _) = std::sync::mpsc::channel();
        let downloader = Downloader {
            retry: Default::default(),
            client: Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .unwrap(),
            control,
            tx,
            source: "official".into(),
        };
        let destination = workspace.path.join("file");
        let result = downloader.fetch(&Download {
            url: format!("{redirect}/first"),
            dest: destination.clone(),
            sha1: Some("a9993e364706816aba3e25717850c26c9cd0d89d".into()),
            size: Some(3),
            alternatives: vec![format!("{limited}/other"), format!("{available}/file")],
        });
        done.store(true, Ordering::Relaxed);
        timer.join().unwrap();
        first.join().unwrap();
        redirected.join().unwrap();
        second.join().unwrap();
        result.unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"abc");
        assert!(!downloader
            .retry
            .remaining(&format!("{limited}/another"))
            .is_zero());
        assert!(!downloader
            .retry
            .remaining(&format!("{redirect}/another"))
            .is_zero());
    }
    #[test]
    fn unhashed_resume_uses_if_range_and_restarts_or_rejects_changed_content() {
        enum Remaining {
            Missing,
            Exact(&'static [u8]),
            AtMost(usize),
        }
        for (tag, response, expected, remaining) in [
            (Some("\"v1\""), "HTTP/1.1 206 Partial Content\r\nETag: \"v1\"\r\nContent-Length: 2\r\nContent-Range: bytes 1-2/3\r\nConnection: close\r\n\r\nbc", Some(&b"abc"[..]), Remaining::Missing),
            (Some("\"v1\""), "HTTP/1.1 200 OK\r\nETag: \"v2\"\r\nContent-Length: 3\r\nConnection: close\r\n\r\nxyz", Some(&b"xyz"[..]), Remaining::Missing),
            (None, "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nxyz", Some(&b"xyz"[..]), Remaining::Missing),
            (Some("\"v1\""), "HTTP/1.1 206 Partial Content\r\nETag: \"v2\"\r\nContent-Length: 2\r\nContent-Range: bytes 1-2/3\r\nConnection: close\r\n\r\nyz", None, Remaining::Missing),
            (None, "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwxyz", None, Remaining::Exact(b"a")),
            (None, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nwxyz\r\n0\r\n\r\n", None, Remaining::AtMost(3)),
            (Some("\"v1\""), "HTTP/1.1 206 Partial Content\r\nETag: \"v1\"\r\nContent-Range: bytes 1-1/3\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nbc\r\n0\r\n\r\n", None, Remaining::AtMost(2)),
            (None, "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n", None, Remaining::Exact(b"a")),
        ] {
            let workspace = crate::test_support::Workspace::new();
            let dest = workspace.path.join("file");
            let partial = workspace.path.join("file.part");
            fs::write(&partial, b"a").unwrap();
            let server = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/file", server.local_addr().unwrap());
            let binding = crate::download_resume_service::ResumeBinding::new(&partial);
            binding.record(&url, tag).unwrap();
            let handle = thread::spawn(move || {
                let mut socket = crate::test_support::accept_http(&server).expect("local HTTP fixture");
                socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut request = vec![];
                let mut byte = [0];
                while socket.read(&mut byte).unwrap() > 0 {
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") { break; }
                }
                let request = String::from_utf8(request).unwrap().to_lowercase();
                if tag.is_some() {
                    assert!(request.contains("range: bytes=1-"));
                    assert!(request.contains("if-range: \"v1\""));
                } else {
                    assert!(!request.contains("range:"));
                }
                socket.write_all(response.as_bytes()).unwrap();
            });
            let (tx, _) = std::sync::mpsc::channel();
            let downloader = Downloader { retry: Default::default(), client: Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).build().unwrap(), control: TaskControl::default(), tx, source:"official".into() };
            let meter = crate::download_progress::Meter::new("file", [Some(3)], &downloader.tx);
            let result = downloader.attempt(&Download { url: url.clone(), dest: dest.clone(), sha1:None, size:Some(3), alternatives:vec![] }, &partial, &meter, 0);
            handle.join().unwrap();
            if let Some(bytes) = expected {
                result.unwrap();
                assert_eq!(fs::read(&dest).unwrap(), bytes);
            } else {
                assert!(result.is_err());
                assert!(!dest.exists());
            }
            match remaining {
                Remaining::Missing => assert!(!partial.exists()),
                Remaining::Exact(bytes) => assert_eq!(fs::read(&partial).unwrap(), bytes),
                Remaining::AtMost(limit) => assert!(fs::metadata(&partial).unwrap().len() <= limit as u64),
            }
            if matches!(remaining, Remaining::Missing) || tag.is_none() {
                assert!(binding.validator(&url).is_none());
            }
        }
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
            retry: Default::default(),
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
