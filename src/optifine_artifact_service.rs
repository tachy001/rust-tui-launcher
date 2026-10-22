use crate::{
    config::Config,
    download::Download,
    metadata::{safe_path, valid_id},
    optifine_archive::{self, Identity},
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Patch,
    Copy,
    Embedded { entry: String },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Origin {
    pub schema: u32,
    pub raw_sha1: String,
    pub raw_url: Option<String>,
    pub jar_id: String,
    pub client_sha1: String,
    pub game_minimum: u64,
    pub game: String,
    pub edition: String,
    pub release: String,
    pub action: Action,
}
fn hash_valid(hash: &str) -> bool {
    hash.len() == 40 && hash.bytes().all(|c| c.is_ascii_hexdigit())
}
impl Origin {
    pub(crate) fn raw_path(&self, root: &Path) -> PathBuf {
        root.join("tui-mc-optifine/installers")
            .join(format!("{}.jar", self.raw_sha1))
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema != 1
            || !hash_valid(&self.raw_sha1)
            || !hash_valid(&self.client_sha1)
            || self.game_minimum == 0
        {
            bail!("OptiFine 来源记录无效");
        }
        valid_id(&self.jar_id)?;
        self.identity().validate()?;
        if let Some(url) = &self.raw_url {
            crate::metadata_cache::validate_url(url)?;
        }
        if let Action::Embedded { entry } = &self.action {
            safe_path(Path::new("installer"), entry)?;
        }
        Ok(())
    }
    fn identity(&self) -> Identity {
        Identity {
            game: self.game.clone(),
            edition: self.edition.clone(),
            release: self.release.clone(),
        }
    }
    fn key(&self) -> Result<String> {
        Ok(format!(
            "{:x}",
            Sha1::digest(serde_json::to_vec(&json!([
                self.raw_sha1,
                self.client_sha1,
                self.action
            ]))?)
        ))
    }
}
pub(crate) trait OptiFineIo {
    fn raw(&self, root: &Path, origin: &Origin) -> Result<PathBuf>;
    fn patch(
        &self,
        config: &Config,
        raw: &Path,
        client: &Path,
        output: &Path,
        minimum: u64,
    ) -> Result<()>;
    fn warning(&self, _: String) {}
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stamp {
    schema: u32,
    key: String,
    pub sha1: String,
    pub size: u64,
}
static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Workspace {
    path: PathBuf,
    parent: PathBuf,
}
impl Workspace {
    fn new(root: &Path) -> Result<Self> {
        let parent = root.join("tui-mc-optifine/work");
        fs::create_dir_all(&parent)?;
        let parent = fs::canonicalize(parent)?;
        let path = parent.join(format!(
            "run-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).context("创建 OptiFine 工作目录失败")?;
        Ok(Self { path, parent })
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if fs::canonicalize(&self.parent).ok().as_ref() == Some(&self.parent)
            && fs::canonicalize(&self.path).ok().as_ref() == Some(&self.path)
            && self.path.parent() == Some(self.parent.as_path())
            && self
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("run-"))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
pub(crate) struct OptiFineArtifactService<'a> {
    io: &'a dyn OptiFineIo,
    config: &'a Config,
    control: &'a TaskControl,
}
impl<'a> OptiFineArtifactService<'a> {
    pub(crate) fn new(
        io: &'a dyn OptiFineIo,
        config: &'a Config,
        control: &'a TaskControl,
    ) -> Self {
        Self {
            io,
            config,
            control,
        }
    }
    pub(crate) fn materialize(
        &self,
        root: &Path,
        origin: &Origin,
        destination: &Path,
        expected: Option<(&str, u64)>,
    ) -> Result<Stamp> {
        self.control.check()?;
        origin.validate()?;
        let key = origin.key()?;
        let stamp_path = root.join("tui-mc-optifine/artifacts").join(format!(
            "{:x}.json",
            Sha1::digest(destination.to_string_lossy().as_bytes())
        ));
        let _lease =
            crate::download_lock::DownloadLease::acquire(destination, self.control, || {})?;
        if let Some((sha1, size)) = expected {
            if !hash_valid(sha1) || size == 0 {
                bail!("OptiFine 工件缺少校验信息");
            }
            if valid(destination, sha1, size)? {
                return Ok(Stamp {
                    schema: 1,
                    key,
                    sha1: sha1.into(),
                    size,
                });
            }
        } else if let Ok(file) = fs::File::open(&stamp_path) {
            if file.metadata()?.len() <= 16384 {
                if let Ok(stamp) = serde_json::from_reader::<_, Stamp>(file) {
                    if stamp.schema == 1
                        && stamp.key == key
                        && hash_valid(&stamp.sha1)
                        && valid(destination, &stamp.sha1, stamp.size)?
                    {
                        return Ok(stamp);
                    }
                }
            }
        }
        let raw = self.io.raw(root, origin)?;
        if !optifine_archive::digest(&raw, self.control)?.eq_ignore_ascii_case(&origin.raw_sha1) {
            bail!("OptiFine 安装包来源校验失败");
        }
        let info = optifine_archive::OptiFineArchive::inspect(&raw, self.control)?;
        if info.identity != origin.identity() {
            bail!("OptiFine 安装包身份与记录不一致");
        }
        let workspace = Workspace::new(root)?;
        let output = workspace.path.join("output.jar");
        match &origin.action {
            Action::Patch => {
                if !info.patcher {
                    bail!("OptiFine 安装包缺少补丁器");
                }
                let client = root
                    .join("versions")
                    .join(&origin.jar_id)
                    .join(format!("{}.jar", origin.jar_id));
                if !optifine_archive::digest(&client, self.control)?
                    .eq_ignore_ascii_case(&origin.client_sha1)
                {
                    bail!("OptiFine 补丁输入客户端已改变，请重新安装 OptiFine");
                }
                self.io
                    .patch(self.config, &raw, &client, &output, origin.game_minimum)?;
            }
            Action::Copy => {
                if info.patcher {
                    bail!("此 OptiFine 包需要运行补丁器，不能直接作为已生成工件复制");
                }
                fs::copy(&raw, &output)?;
            }
            Action::Embedded { entry } => {
                let wrapper = info.wrapper.context("OptiFine 缺少记录的 LaunchWrapper")?;
                if wrapper.entry != *entry {
                    bail!("OptiFine LaunchWrapper 条目与记录不一致");
                }
                let mut archive = crate::installer_archive::open(&raw, self.control)?;
                fs::write(
                    &output,
                    optifine_archive::bytes(&mut archive, entry, 16 * 1024 * 1024, self.control)?,
                )?;
            }
        }
        self.control.check()?;
        let normalized = workspace.path.join("normalized.jar");
        optifine_archive::normalize(&output, &normalized, self.control)?;
        let stamp = Stamp {
            schema: 1,
            key,
            sha1: optifine_archive::digest(&normalized, self.control)?,
            size: fs::metadata(&normalized)?.len(),
        };
        if expected.is_some_and(|(hash, size)| {
            !hash.eq_ignore_ascii_case(&stamp.sha1) || size != stamp.size
        }) {
            bail!("重建 OptiFine 内容与原记录不同，原工件未修改");
        }
        self.control.check()?;
        crate::storage::atomic_copy(destination, &mut fs::File::open(normalized)?)?;
        if let Err(error) = crate::storage::atomic_write(&stamp_path, &serde_json::to_vec(&stamp)?)
        {
            self.io
                .warning(format!("OptiFine 已准备，但未保存复用记录：{error:#}"));
        }
        Ok(stamp)
    }
    pub(crate) fn library(&self, root: &Path, name: &str, origin: Origin) -> Result<Value> {
        let path = crate::version::maven_path(name)?;
        let destination = safe_path(&root.join("libraries"), &path)?;
        let stamp = self.materialize(root, &origin, &destination, None)?;
        Ok(
            json!({"name":name,"downloads":{"artifact":{"path":path,"url":origin.raw_url.as_deref().unwrap_or("https://optifine.net/downloads"),"sha1":stamp.sha1,"size":stamp.size}},"tuiMcOptiFine":origin}),
        )
    }
    pub(crate) fn prepare(
        &self,
        root: &Path,
        library: &Value,
        client_owner: Option<&str>,
    ) -> Result<()> {
        let Some(origin) = library.get("tuiMcOptiFine") else {
            return Ok(());
        };
        let origin: Origin = serde_json::from_value(origin.clone())?;
        if client_owner != Some(origin.jar_id.as_str()) {
            bail!("OptiFine 来源记录与当前客户端归属不一致");
        }
        let entry = &library["downloads"]["artifact"];
        let path = safe_path(
            &root.join("libraries"),
            entry["path"].as_str().context("OptiFine 缺少工件路径")?,
        )?;
        self.materialize(
            root,
            &origin,
            &path,
            Some((
                entry["sha1"].as_str().context("OptiFine 缺少 SHA-1")?,
                entry["size"].as_u64().context("OptiFine 缺少大小")?,
            )),
        )?;
        Ok(())
    }
}
fn valid(path: &Path, hash: &str, size: u64) -> Result<bool> {
    Ok(crate::download::valid_file(path, Some(hash), Some(size))?
        && zip::ZipArchive::new(fs::File::open(path)?).is_ok())
}
impl OptiFineIo for crate::backend::Backend {
    fn raw(&self, root: &Path, origin: &Origin) -> Result<PathBuf> {
        let path = origin.raw_path(root);
        if let Some(url) = &origin.raw_url {
            self.downloader().fetch(&Download {
                url: url.clone(),
                dest: path.clone(),
                sha1: Some(origin.raw_sha1.clone()),
                size: None,
                alternatives: vec![],
            })?;
        } else if !crate::download::valid_file(&path, Some(&origin.raw_sha1), None)? {
            bail!("本地 OptiFine 安装包缓存缺失，请重新导入原安装包");
        }
        Ok(path)
    }
    fn patch(
        &self,
        config: &Config,
        raw: &Path,
        client: &Path,
        output: &Path,
        minimum: u64,
    ) -> Result<()> {
        use crate::installer_java_service::{
            ClassPathEntrySource, InstallerJavaRequirement, InstallerJavaService,
        };
        let requirement = InstallerJavaRequirement::inspect(
            &ClassPathEntrySource("optifine.Patcher"),
            raw,
            minimum,
            &self.control,
        )?;
        let runtime = InstallerJavaService::new(self).resolve(config, &requirement)?;
        let logs = raw
            .parent()
            .and_then(Path::parent)
            .context("OptiFine 缓存目录无效")?
            .join("logs");
        fs::create_dir_all(&logs)?;
        let name = output
            .parent()
            .and_then(Path::file_name)
            .context("OptiFine 工作目录缺少名称")?
            .to_string_lossy();
        let log = logs.join(format!("{name}.log"));
        self.log(format!("OptiFine 补丁日志：{}", log.display()));
        let file = fs::File::create(&log)?;
        let mut tail = crate::logs::LogTail::open(&log)?;
        let _ = self.tx.send(crate::backend::Message::Pauseable(false));
        let result = (|| -> Result<()> {
            self.control.check()?;
            let child = std::process::Command::new(&runtime.path)
                .args(["-Djava.awt.headless=true", "-Dfile.encoding=UTF-8", "-cp"])
                .arg(crate::paths::for_java(raw))
                .arg("optifine.Patcher")
                .arg(crate::paths::for_java(client))
                .arg(crate::paths::for_java(raw))
                .arg(crate::paths::for_java(output))
                .current_dir(output.parent().context("OptiFine 工作目录缺失")?)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(file.try_clone()?))
                .stderr(std::process::Stdio::from(file))
                .spawn()?;
            let mut child = crate::process_service::ManagedProcess(child);
            match crate::process_service::ProcessMonitor::new(&self.control).run(
                &mut child,
                &mut tail,
                |line| self.log(format!("OptiFine · {line}")),
                |warning| self.log(warning),
            )? {
                crate::process_service::ProcessOutcome::Exited(status) if status.success() => {
                    Ok(())
                }
                crate::process_service::ProcessOutcome::Exited(status) => {
                    bail!("OptiFine 补丁器失败：{status}")
                }
                crate::process_service::ProcessOutcome::Cancelled => bail!("OptiFine 安装已取消"),
            }
        })();
        let _ = self.tx.send(crate::backend::Message::Pauseable(true));
        result
    }
    fn warning(&self, message: String) {
        self.log(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Io {
        raw: PathBuf,
        calls: Cell<usize>,
        fail: bool,
        cancel: Option<TaskControl>,
    }
    impl OptiFineIo for Io {
        fn raw(&self, _: &Path, _: &Origin) -> Result<PathBuf> {
            Ok(self.raw.clone())
        }
        fn patch(&self, _: &Config, raw: &Path, _: &Path, output: &Path, _: u64) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                bail!("synthetic patch failure");
            }
            if let Some(control) = &self.cancel {
                control.cancel();
            }
            fs::copy(raw, output)?;
            Ok(())
        }
    }
    fn setup(root: &Path, control: &TaskControl) -> (Io, Origin) {
        let raw = root.join("raw.jar");
        crate::optifine_archive::fixtures::installer(&raw, "1.12.2", true, true);
        let client = root.join("versions/game/game.jar");
        fs::create_dir_all(client.parent().unwrap()).unwrap();
        fs::write(&client, b"client fixture").unwrap();
        let origin = Origin {
            schema: 1,
            raw_sha1: optifine_archive::digest(&raw, control).unwrap(),
            raw_url: None,
            jar_id: "game".into(),
            client_sha1: optifine_archive::digest(&client, control).unwrap(),
            game_minimum: 8,
            game: "1.12.2".into(),
            edition: "HD_U".into(),
            release: "I6".into(),
            action: Action::Patch,
        };
        (
            Io {
                raw,
                calls: Cell::new(0),
                fail: false,
                cancel: None,
            },
            origin,
        )
    }
    #[test]
    fn cached_derived_artifact_is_reused_and_rebuilt_with_the_original_checksum() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let config = Config::default();
        let (io, origin) = setup(&workspace.path, &control);
        let service = OptiFineArtifactService::new(&io, &config, &control);
        let library = service
            .library(&workspace.path, "optifine:OptiFine:test", origin.clone())
            .unwrap();
        let path = workspace
            .path
            .join("libraries/optifine/OptiFine/test/OptiFine-test.jar");
        let original = fs::read(&path).unwrap();
        service
            .library(&workspace.path, "optifine:OptiFine:test", origin)
            .unwrap();
        assert_eq!(io.calls.get(), 1);
        fs::write(&path, b"corrupt").unwrap();
        service
            .prepare(&workspace.path, &library, Some("game"))
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(io.calls.get(), 2);
        fs::remove_file(&io.raw).unwrap();
        service
            .prepare(&workspace.path, &library, Some("game"))
            .unwrap();
        assert_eq!(io.calls.get(), 2);
    }
    #[test]
    fn regeneration_failure_or_bad_expected_hash_preserves_the_existing_file() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let config = Config::default();
        let (mut io, origin) = setup(&workspace.path, &control);
        let destination = workspace.path.join("libraries/preserve.jar");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"preserve").unwrap();
        io.fail = true;
        let service = OptiFineArtifactService::new(&io, &config, &control);
        assert!(service
            .materialize(&workspace.path, &origin, &destination, None)
            .is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"preserve");
        io.fail = false;
        let service = OptiFineArtifactService::new(&io, &config, &control);
        assert!(service
            .materialize(
                &workspace.path,
                &origin,
                &destination,
                Some((&"0".repeat(40), 1))
            )
            .is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"preserve");
        assert!(fs::read_dir(workspace.path.join("tui-mc-optifine/work"))
            .unwrap()
            .next()
            .is_none());
    }
    #[test]
    fn input_identity_owner_and_cancellation_are_checked_before_publication() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let config = Config::default();
        let (mut io, origin) = setup(&workspace.path, &control);
        let service = OptiFineArtifactService::new(&io, &config, &control);
        let library = service
            .library(&workspace.path, "optifine:OptiFine:test", origin.clone())
            .unwrap();
        assert!(service
            .prepare(&workspace.path, &library, Some("other"))
            .is_err());
        assert_eq!(io.calls.get(), 1);
        let target = workspace.path.join("different.jar");
        fs::write(workspace.path.join("versions/game/game.jar"), b"changed").unwrap();
        assert!(service
            .materialize(&workspace.path, &origin, &target, None)
            .is_err());
        assert_eq!(io.calls.get(), 1);
        assert!(!target.exists());
        fs::write(
            workspace.path.join("versions/game/game.jar"),
            b"client fixture",
        )
        .unwrap();
        io.cancel = Some(control.clone());
        assert!(OptiFineArtifactService::new(&io, &config, &control)
            .materialize(&workspace.path, &origin, &target, None)
            .is_err());
        assert!(!target.exists());
    }
    #[test]
    fn embedded_wrapper_is_restored_without_running_the_patcher() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let config = Config::default();
        let (io, mut origin) = setup(&workspace.path, &control);
        origin.action = Action::Embedded {
            entry: "launchwrapper-of-2.3.jar".into(),
        };
        let destination = workspace.path.join("wrapper.jar");
        OptiFineArtifactService::new(&io, &config, &control)
            .materialize(&workspace.path, &origin, &destination, None)
            .unwrap();
        assert_eq!(io.calls.get(), 0);
        let archive = crate::installer_archive::open(&destination, &control).unwrap();
        assert!(archive
            .file_names()
            .any(|name| name == "net/minecraft/launchwrapper/Launch.class"));
    }
}
