use crate::{
    config::Config,
    download::valid_file_with_control,
    download_lock::DownloadLease,
    installation_service::RuntimeInstallSession,
    metadata::{safe_path, valid_id},
    storage::atomic_write,
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const MANAGED_DIR: &str = ".tui-managed";
static SERIAL: AtomicU64 = AtomicU64::new(0);
#[derive(Serialize, Deserialize)]
struct CurrentRuntime {
    directory: String,
    manifest_sha1: String,
}

fn current(repository: &Path) -> Result<Option<(PathBuf, CurrentRuntime)>> {
    let bytes = match fs::read(repository.join("current.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("读取当前 Java 运行时记录失败"),
    };
    let pointer: CurrentRuntime =
        serde_json::from_slice(&bytes).context("当前 Java 运行时记录格式损坏")?;
    if pointer.manifest_sha1.len() != 40
        || !pointer
            .manifest_sha1
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("当前 Java 记录缺少有效清单 SHA-1");
    }
    valid_id(&pointer.directory)?;
    if !pointer.directory.starts_with("generation-") {
        bail!("当前 Java 记录指向非发布目录");
    }
    let directory =
        fs::canonicalize(repository.join(&pointer.directory)).context("已发布 Java 目录缺失")?;
    if directory.parent() != Some(fs::canonicalize(repository)?.as_path()) {
        bail!("已发布 Java 目录越过组件仓库");
    }
    if directory.join(".installing").try_exists()? {
        bail!("当前 Java 尚未安装完成");
    }
    if fs::read_to_string(directory.join(".complete"))? != pointer.manifest_sha1 {
        bail!("当前 Java 完成记录与清单不一致");
    }
    Ok(Some((crate::paths::for_java(&directory), pointer)))
}

pub(crate) fn executable(root: &Path) -> PathBuf {
    let filename = if cfg!(windows) { "java.exe" } else { "java" };
    let direct = root.join("bin").join(filename);
    if direct.is_file() {
        direct
    } else {
        root.join("jre.bundle/Contents/Home/bin").join(filename)
    }
}

/// Only published generations are discoverable; legacy installations remain independent.
pub(crate) fn installed_paths(config: &Config, control: &TaskControl) -> Result<Vec<PathBuf>> {
    let managed = config.game_dir.join("runtime").join(MANAGED_DIR);
    let components = match fs::read_dir(&managed) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error).context("读取托管 Java 仓库失败"),
    };
    let mut result = vec![];
    for component in components.flatten() {
        control.check()?;
        if !component.file_type()?.is_dir() {
            continue;
        }
        let Ok(platforms) = fs::read_dir(component.path()) else {
            continue;
        };
        for platform in platforms.flatten() {
            control.check()?;
            if !platform.file_type()?.is_dir() {
                continue;
            }
            // As with external Java discovery, an unusable candidate is skipped, never rewritten.
            if let Ok(Some((root, _))) = current(&platform.path()) {
                let path = executable(&root);
                if path.is_file() {
                    result.push(path);
                }
            }
        }
    }
    Ok(result)
}

/// Builds privately, then switches a small atomic pointer without replacing any in-use JVM files.
pub(crate) struct RuntimeGeneration<'a> {
    repository: PathBuf,
    stage: PathBuf,
    fingerprint: String,
    session: RuntimeInstallSession<'a>,
    control: &'a TaskControl,
    _lease: DownloadLease,
}
impl<'a> RuntimeGeneration<'a> {
    pub(crate) fn begin(
        repository: &Path,
        fingerprint: &str,
        control: &'a TaskControl,
        on_wait: impl FnMut(),
    ) -> Result<Self> {
        if fingerprint.len() != 40 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("Java 清单 SHA-1 无效");
        }
        let lease =
            DownloadLease::acquire(&repository.join(".generation-install"), control, on_wait)?;
        let fingerprint = fingerprint.to_ascii_lowercase();
        let stage = repository.join(format!(".pending-{fingerprint}"));
        let session = RuntimeInstallSession::begin(&stage, &fingerprint, control, || {})?;
        Ok(Self {
            repository: repository.into(),
            stage,
            fingerprint,
            session,
            control,
            _lease: lease,
        })
    }
    pub(crate) fn root(&self) -> &Path {
        &self.stage
    }
    pub(crate) fn destination(&self, name: &str) -> Result<PathBuf> {
        self.session.destination(name)
    }
    pub(crate) fn reusable(&self, files: &Map<String, Value>) -> Result<Option<PathBuf>> {
        self.control.check()?;
        let Some((root, pointer)) = current(&self.repository)? else {
            return Ok(None);
        };
        if pointer.manifest_sha1 != self.fingerprint {
            return Ok(None);
        }
        for (name, file) in files {
            self.control.check()?;
            self.session.destination(name)?;
            let path = safe_path(&root, name)?;
            match file["type"].as_str() {
                Some("directory") if path.is_dir() => {}
                Some("file") => {
                    let raw = &file["downloads"]["raw"];
                    let Some(hash) = raw["sha1"].as_str() else {
                        return Ok(None);
                    };
                    let Some(size) = raw["size"].as_u64() else {
                        return Ok(None);
                    };
                    if !valid_file_with_control(&path, Some(hash), Some(size), self.control)? {
                        return Ok(None);
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if file["executable"] == true
                            && fs::metadata(&path)?.permissions().mode() & 0o111 == 0
                        {
                            return Ok(None);
                        }
                    }
                }
                Some("link") => {
                    let Some(target) = file["target"].as_str() else {
                        return Ok(None);
                    };
                    let source = crate::java::checked_link_target(&root, &path, target)?;
                    let canonical = match fs::canonicalize(&source) {
                        Ok(source) => source,
                        Err(_) => return Ok(None),
                    };
                    if !canonical.starts_with(fs::canonicalize(&root)?) {
                        return Ok(None);
                    }
                    #[cfg(unix)]
                    if fs::read_link(&path).ok().as_deref() != Some(Path::new(target)) {
                        return Ok(None);
                    }
                    #[cfg(windows)]
                    {
                        let relative = source
                            .strip_prefix(&root)?
                            .to_string_lossy()
                            .replace('\\', "/");
                        let raw = &files.get(&relative).unwrap_or(&Value::Null)["downloads"]["raw"];
                        let (Some(hash), Some(size)) = (raw["sha1"].as_str(), raw["size"].as_u64())
                        else {
                            return Ok(None);
                        };
                        if !valid_file_with_control(&path, Some(hash), Some(size), self.control)? {
                            return Ok(None);
                        }
                    }
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(root))
    }
    pub(crate) fn publish(self) -> Result<PathBuf> {
        self.publish_using(atomic_write)
    }
    fn publish_using(
        self,
        write_pointer: impl FnOnce(&Path, &[u8]) -> Result<()>,
    ) -> Result<PathBuf> {
        let Self {
            repository,
            stage,
            fingerprint,
            session,
            control,
            _lease,
        } = self;
        session.finish()?;
        let name = format!(
            "generation-{}-{}-{}-{}",
            &fingerprint[..12],
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        let published = repository.join(&name);
        fs::rename(&stage, &published).context("无法移交准备完成的 Java 目录；当前运行时未改变")?;
        control.check()?;
        write_pointer(
            &repository.join("current.json"),
            &serde_json::to_vec(&CurrentRuntime {
                directory: name,
                manifest_sha1: fingerprint,
            })?,
        )
        .context("无法发布新 Java 运行时；原运行时记录已保留")?;
        Ok(crate::paths::for_java(&fs::canonicalize(&published)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
    const NEXT: &str = "1123456789abcdef0123456789abcdef01234567";
    fn write_binary(generation: &RuntimeGeneration<'_>, contents: &[u8]) {
        let path =
            generation
                .root()
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" });
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    #[test]
    fn publishing_preserves_old_files_and_pending_or_orphan_directories_are_not_discovered() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            ..Default::default()
        };
        let repository = workspace
            .path
            .join("runtime")
            .join(MANAGED_DIR)
            .join("component/platform");
        let control = TaskControl::default();
        let first = RuntimeGeneration::begin(&repository, HASH, &control, || {}).unwrap();
        write_binary(&first, b"old runtime");
        assert!(installed_paths(&config, &control).unwrap().is_empty());
        let old = first.publish().unwrap();
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1); // Allow readers but deny replacement/deletion of this in-use file.
        }
        let _old_in_use = options.open(executable(&old)).unwrap();
        let next = RuntimeGeneration::begin(&repository, NEXT, &control, || {}).unwrap();
        write_binary(&next, b"new runtime");
        assert_eq!(
            installed_paths(&config, &control).unwrap(),
            [executable(&old)]
        );
        let new = next.publish().unwrap();
        assert_eq!(fs::read(executable(&old)).unwrap(), b"old runtime");
        assert_eq!(
            installed_paths(&config, &control).unwrap(),
            [executable(&new)]
        );
    }
    #[test]
    fn interrupted_build_keeps_published_runtime_and_partial_files_can_resume() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let first = RuntimeGeneration::begin(&workspace.path, HASH, &control, || {}).unwrap();
        write_binary(&first, b"old");
        let old = first.publish().unwrap();
        let next = RuntimeGeneration::begin(&workspace.path, NEXT, &control, || {}).unwrap();
        fs::write(next.root().join("partial.part"), b"partial").unwrap();
        drop(next);
        assert_eq!(current(&workspace.path).unwrap().unwrap().0, old);
        let resumed = RuntimeGeneration::begin(&workspace.path, NEXT, &control, || {}).unwrap();
        assert_eq!(
            fs::read(resumed.root().join("partial.part")).unwrap(),
            b"partial"
        );
        control.cancel();
        assert!(resumed.publish().is_err());
        assert_eq!(current(&workspace.path).unwrap().unwrap().0, old);
    }
    #[test]
    fn cancelled_reuse_is_not_reported_as_a_missing_runtime() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let generation = RuntimeGeneration::begin(&workspace.path, HASH, &control, || {}).unwrap();
        control.cancel();
        let error = generation.reusable(&Map::new()).unwrap_err();
        assert!(error.to_string().contains("取消"));
        assert!(current(&workspace.path).unwrap().is_none());
    }
    #[test]
    fn healthy_generation_is_reused_only_when_all_declared_file_hashes_match() {
        use sha1::{Digest, Sha1};
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let generation = RuntimeGeneration::begin(&workspace.path, HASH, &control, || {}).unwrap();
        let name = if cfg!(windows) {
            "bin/java.exe"
        } else {
            "bin/java"
        };
        write_binary(&generation, b"java");
        let root = generation.publish().unwrap();
        let manifest = serde_json::json!({name:{"type":"file","downloads":{"raw":{"sha1":format!("{:x}", Sha1::digest(b"java")),"size":4}}}});
        let retry = RuntimeGeneration::begin(&workspace.path, HASH, &control, || {}).unwrap();
        assert_eq!(
            retry.reusable(manifest.as_object().unwrap()).unwrap(),
            Some(root.clone())
        );
        fs::write(root.join(name), b"fake").unwrap();
        assert!(retry
            .reusable(manifest.as_object().unwrap())
            .unwrap()
            .is_none());
    }
    #[test]
    fn pointer_failure_does_not_replace_the_previous_record_or_old_runtime() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let first = RuntimeGeneration::begin(&workspace.path, HASH, &control, || {}).unwrap();
        write_binary(&first, b"original");
        let old = first.publish().unwrap();
        let old_pointer = fs::read(workspace.path.join("current.json")).unwrap();
        let generation = RuntimeGeneration::begin(&workspace.path, NEXT, &control, || {}).unwrap();
        write_binary(&generation, b"prepared");
        assert!(generation
            .publish_using(|_, _| anyhow::bail!("simulated pointer write failure"))
            .is_err());
        assert_eq!(
            fs::read(workspace.path.join("current.json")).unwrap(),
            old_pointer
        );
        assert_eq!(current(&workspace.path).unwrap().unwrap().0, old);
        assert_eq!(fs::read(executable(&old)).unwrap(), b"original");
    }
}
