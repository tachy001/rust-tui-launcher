use crate::{instance_service::InstanceMutation, metadata::valid_id, task::TaskControl};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    fs,
    path::{Path, PathBuf},
};
struct PreparedClient {
    entry: crate::download::Download,
    staged: bool,
}

pub(crate) trait ProfilePublisher {
    /// Publication is atomic: an error must leave the previous profile intact.
    fn publish(&self, path: &Path, bytes: &[u8]) -> Result<()>;
}

struct FilePublisher;
impl ProfilePublisher for FilePublisher {
    fn publish(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        crate::storage::atomic_write(path, bytes)
    }
}
/// Dependencies can be prepared using a candidate without exposing it to discovery.
/// Partial downloads remain in this owned draft for a retry with the same profile.
pub(crate) struct InstanceDraft {
    root: PathBuf,
    id: String,
    profile: Value,
    bytes: Vec<u8>,
    stage: PathBuf,
    client: Option<PreparedClient>,
}
impl InstanceDraft {
    pub(crate) fn begin(
        root: &Path,
        id: &str,
        mut profile: Value,
        mutation: &InstanceMutation,
        control: &TaskControl,
    ) -> Result<Self> {
        mutation.require(root, id)?;
        valid_id(id)?;
        control.check()?;
        if crate::instance_recovery::InstanceRecovery::pending(root, id)? {
            bail!("上次安装提交的恢复记录尚未清理，拒绝覆盖");
        }
        if !profile.is_object() || profile["id"].as_str() != Some(id) {
            bail!("安装配置与目标实例 ID 不一致");
        }
        let directory = root.join("versions").join(id);
        let previous = match fs::read(directory.join(format!("{id}.json"))) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("无法读取已有实例配置"),
        };
        if previous
            .as_ref()
            .is_some_and(|value| value["tuiMcIsolated"] == true)
        {
            profile["tuiMcIsolated"] = true.into();
        }
        let bytes = serde_json::to_vec_pretty(&profile)?;
        let drafts = directory.join(".tui-mc-install-drafts");
        let stage = drafts.join(format!("{:x}", Sha1::digest(&bytes)));
        for path in [&drafts, &stage] {
            if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
                bail!("安装暂存目录不能为符号链接");
            }
            fs::create_dir_all(path)?;
        }
        if stage.join("client.previous.jar").exists() {
            bail!(
                "发现上次提交遗留的客户端备份，请先检查恢复：{}",
                stage.display()
            );
        }
        crate::storage::atomic_write(&stage.join("profile.json"), &bytes)?;
        Ok(Self {
            root: root.into(),
            id: id.into(),
            profile,
            bytes,
            stage,
            client: None,
        })
    }
    pub(crate) fn profile(&self) -> &Value {
        &self.profile
    }
    pub(crate) fn client_path(&self) -> PathBuf {
        self.stage.join("client.jar")
    }
    fn valid_client(entry: &crate::download::Download, path: &Path) -> Result<bool> {
        if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            bail!("客户端暂存或发布路径不能为符号链接");
        }
        let mut requested = entry.clone();
        requested.dest = path.into();
        crate::download::cached(&requested)
    }
    fn require_client(&self, path: &Path, mutation: &InstanceMutation) -> Result<()> {
        let versions = self.root.join("versions");
        let relative = path
            .strip_prefix(&versions)
            .context("客户端不在游戏版本目录内")?;
        let parts: Vec<_> = relative.components().collect();
        if parts.len() != 2 {
            bail!("客户端路径结构无效");
        }
        let owner = parts[0]
            .as_os_str()
            .to_str()
            .context("客户端 ID 不能表示为 Unicode")?;
        valid_id(owner)?;
        if path != versions.join(owner).join(format!("{owner}.jar")) {
            bail!("客户端文件名与归属版本不一致");
        }
        mutation.require_protected(&self.root, owner)
    }
    pub(crate) fn prepare_client(
        &mut self,
        entry: &crate::download::Download,
        fetch: impl FnOnce(&crate::download::Download) -> Result<()>,
        mutation: &InstanceMutation,
        control: &TaskControl,
    ) -> Result<()> {
        self.require_client(&entry.dest, mutation)?;
        control.check()?;
        if Self::valid_client(entry, &entry.dest)? {
            self.client = Some(PreparedClient {
                entry: entry.clone(),
                staged: false,
            });
            return Ok(());
        }
        if entry.dest
            != self
                .root
                .join("versions")
                .join(&self.id)
                .join(format!("{}.jar", self.id))
        {
            bail!("引用的父版本客户端尚未就绪，请先修复父版本；安装草稿不会替换其他实例的客户端");
        }
        let mut staged = entry.clone();
        staged.dest = self.client_path();
        fetch(&staged)?;
        control.check()?;
        if !Self::valid_client(&staged, &staged.dest)? {
            bail!("暂存客户端校验未通过");
        }
        self.client = Some(PreparedClient {
            entry: entry.clone(),
            staged: true,
        });
        Ok(())
    }
    pub(crate) fn commit(
        self,
        mutation: &InstanceMutation,
        control: &TaskControl,
    ) -> Result<Option<String>> {
        self.commit_with(mutation, control, &FilePublisher)
    }
    fn commit_with(
        self,
        mutation: &InstanceMutation,
        control: &TaskControl,
        publisher: &dyn ProfilePublisher,
    ) -> Result<Option<String>> {
        mutation.require(&self.root, &self.id)?;
        control.check()?;
        let prepared = self.client.as_ref().context("安装客户端尚未准备完毕")?;
        let client = &prepared.entry.dest;
        self.require_client(client, mutation)?;
        let profile_path = self
            .root
            .join("versions")
            .join(&self.id)
            .join(format!("{}.json", self.id));
        if !prepared.staged {
            if !Self::valid_client(&prepared.entry, client)? {
                bail!("客户端在提交前发生变化，请重试");
            }
            control.check()?;
            publisher.publish(&profile_path, &self.bytes)?;
            let _ = fs::remove_file(self.stage.join("profile.json"));
            let _ = fs::remove_dir(&self.stage);
            return Ok(None);
        }
        let staged = self.client_path();
        if !Self::valid_client(&prepared.entry, &staged)? {
            bail!("暂存客户端尚未就绪，拒绝发布配置");
        }
        let backup = self.stage.join("client.previous.jar");
        if backup.exists() {
            bail!("客户端备份已经存在，拒绝覆盖");
        }
        let previous = match fs::symlink_metadata(client) {
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() {
                    bail!("原客户端必须为普通文件");
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error).context("无法检查已有客户端"),
        };
        fs::create_dir_all(client.parent().context("客户端没有父目录")?)?;
        let journal = crate::instance_recovery::InstanceRecovery::prepare(
            &self.root,
            &self.id,
            &self.stage,
            &self.bytes,
        )?;
        let result = (|| -> Result<()> {
            if control.cancelled() {
                bail!("安装已取消");
            }
            if previous {
                fs::rename(client, &backup).context("无法备份已有客户端")?;
            }
            fs::rename(&staged, client).context("无法提交客户端")?;
            // Do not pause with mixed files in the commit critical section.
            if control.cancelled() {
                bail!("安装已取消，正在恢复原客户端");
            }
            publisher.publish(&profile_path, &self.bytes)?;
            Ok(())
        })();
        if let Err(error) = result {
            return match journal.rollback() {
                Ok(None) => Err(error),
                Ok(Some(warning)) => Err(error).context(warning),
                Err(rollback) => {
                    Err(error).context(format!("回滚未完成，恢复记录与备份已保留：{rollback:#}"))
                }
            };
        }
        journal.complete()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::Backend, config::Config, download::Download};
    use std::{
        cell::Cell,
        io::{Cursor, Write},
        sync::mpsc,
        time::Duration,
    };
    fn jar(label: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("fixture", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(label.as_bytes()).unwrap();
        writer.finish().unwrap().into_inner()
    }
    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }
    fn profile(bytes: &[u8]) -> Value {
        let index = br#"{"objects":{}}"#;
        serde_json::json!({"id":"game","mainClass":"net.minecraft.client.main.Main","libraries":[],
            "downloads":{"client":{"url":"https://must-not-request.invalid/client.jar","sha1":hash(bytes),"size":bytes.len()}},
            "assetIndex":{"id":"empty","url":"https://must-not-request.invalid/index.json","sha1":hash(index),"size":index.len()}})
    }
    fn entry(root: &Path, bytes: &[u8]) -> Download {
        Download {
            url: "https://must-not-request.invalid/client.jar".into(),
            dest: root.join("versions/game/game.jar"),
            sha1: Some(hash(bytes)),
            size: Some(bytes.len() as u64),
            alternatives: vec![],
        }
    }
    fn old(root: &Path, bytes: &[u8]) -> Vec<u8> {
        let dir = root.join("versions/game");
        fs::create_dir_all(&dir).unwrap();
        let metadata = br#"{"id":"game","tuiMcIsolated":true,"tag":"original"}"#.to_vec();
        fs::write(dir.join("game.json"), &metadata).unwrap();
        fs::write(dir.join("game.jar"), bytes).unwrap();
        metadata
    }
    fn backend() -> Backend {
        let (tx, _) = mpsc::channel();
        let mut backend = Backend::new(tx).unwrap();
        // An unexpected cache miss cannot reach an external service.
        backend.client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(100))
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .build()
            .unwrap();
        backend
    }
    #[test]
    fn failed_publication_restores_old_client_and_retry_reuses_staging() {
        struct Fail;
        impl ProfilePublisher for Fail {
            fn publish(&self, _: &Path, _: &[u8]) -> Result<()> {
                bail!("injected publication failure")
            }
        }
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        let old_client = jar("old");
        let new_client = jar("new");
        let previous = old(root, &old_client);
        let control = TaskControl::default();
        let mutation = InstanceMutation::begin(root, "game").unwrap();
        let writes = Cell::new(0);
        let mut draft =
            InstanceDraft::begin(root, "game", profile(&new_client), &mutation, &control).unwrap();
        let staged = draft.client_path();
        draft
            .prepare_client(
                &entry(root, &new_client),
                |download| {
                    writes.set(writes.get() + 1);
                    fs::write(&download.dest, &new_client)?;
                    Ok(())
                },
                &mutation,
                &control,
            )
            .unwrap();
        assert_eq!(
            fs::read(root.join("versions/game/game.jar")).unwrap(),
            old_client
        );
        assert!(draft.commit_with(&mutation, &control, &Fail).is_err());
        assert_eq!(
            fs::read(root.join("versions/game/game.json")).unwrap(),
            previous
        );
        assert_eq!(
            fs::read(root.join("versions/game/game.jar")).unwrap(),
            old_client
        );
        assert_eq!(fs::read(&staged).unwrap(), new_client);
        let mut retry =
            InstanceDraft::begin(root, "game", profile(&new_client), &mutation, &control).unwrap();
        retry
            .prepare_client(
                &entry(root, &new_client),
                |download| {
                    assert!(crate::download::valid_file(
                        &download.dest,
                        download.sha1.as_deref(),
                        download.size
                    )?);
                    Ok(())
                },
                &mutation,
                &control,
            )
            .unwrap();
        retry.commit(&mutation, &control).unwrap();
        assert_eq!(writes.get(), 1);
        assert_eq!(
            fs::read(root.join("versions/game/game.jar")).unwrap(),
            new_client
        );
        let published: Value =
            serde_json::from_slice(&fs::read(root.join("versions/game/game.json")).unwrap())
                .unwrap();
        assert_eq!(published["tuiMcIsolated"], true);
    }
    #[test]
    fn compatible_client_is_reused_without_fetch_or_replacement() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        let client = jar("current");
        old(root, &client);
        let mutation = InstanceMutation::begin(root, "game").unwrap();
        let control = TaskControl::default();
        let mut draft =
            InstanceDraft::begin(root, "game", profile(&client), &mutation, &control).unwrap();
        let staged = draft.client_path();
        draft
            .prepare_client(
                &entry(root, &client),
                |_| panic!("must not download cached client"),
                &mutation,
                &control,
            )
            .unwrap();
        assert!(!staged.exists());
        draft.commit(&mutation, &control).unwrap();
        assert_eq!(
            fs::read(root.join("versions/game/game.jar")).unwrap(),
            client
        );
    }
    #[test]
    fn cancellation_and_unprotected_client_never_publish_metadata() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        let previous = old(root, &jar("old"));
        let bytes = jar("new");
        let mutation = InstanceMutation::begin(root, "game").unwrap();
        let control = TaskControl::default();
        let mut draft =
            InstanceDraft::begin(root, "game", profile(&bytes), &mutation, &control).unwrap();
        let mut unprotected = entry(root, &bytes);
        unprotected.dest = root.join("versions/other/other.jar");
        assert!(draft
            .prepare_client(
                &unprotected,
                |_| panic!("unprotected write"),
                &mutation,
                &control
            )
            .is_err());
        draft
            .prepare_client(
                &entry(root, &bytes),
                |download| {
                    fs::write(&download.dest, &bytes)?;
                    Ok(())
                },
                &mutation,
                &control,
            )
            .unwrap();
        control.cancel();
        assert!(draft.commit(&mutation, &control).is_err());
        assert_eq!(
            fs::read(root.join("versions/game/game.json")).unwrap(),
            previous
        );
    }
    #[test]
    fn unfinished_draft_is_not_discovered_and_stale_backup_is_preserved() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        let client = jar("new");
        let config = Config {
            game_dir: root.clone(),
            ..Config::default()
        };
        let control = TaskControl::default();
        let mutation = InstanceMutation::begin(root, "game").unwrap();
        let draft =
            InstanceDraft::begin(root, "game", profile(&client), &mutation, &control).unwrap();
        assert!(crate::backend::installed(&config).is_empty());
        fs::write(draft.stage.join("client.previous.jar"), b"preserve backup").unwrap();
        assert!(InstanceDraft::begin(root, "game", profile(&client), &mutation, &control).is_err());
        assert_eq!(
            fs::read(draft.stage.join("client.previous.jar")).unwrap(),
            b"preserve backup"
        );
    }
    #[test]
    fn backend_dependency_failure_keeps_original_profile_and_client() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            ..Config::default()
        };
        let root = config.root().unwrap();
        let old_client = jar("old");
        let bytes = jar("new");
        let previous = old(&root, &old_client);
        let mut candidate = profile(&bytes);
        candidate.as_object_mut().unwrap().remove("libraries");
        let mut mutation = InstanceMutation::begin(&root, "game").unwrap();
        let control = TaskControl::default();
        let draft =
            InstanceDraft::begin(&root, "game", candidate.clone(), &mutation, &control).unwrap();
        fs::write(draft.client_path(), &bytes).unwrap();
        assert!(backend()
            .install_profile_with_session(&config, "game", candidate, &mut mutation)
            .is_err());
        assert_eq!(
            fs::read(root.join("versions/game/game.json")).unwrap(),
            previous
        );
        assert_eq!(
            fs::read(root.join("versions/game/game.jar")).unwrap(),
            old_client
        );
        assert_eq!(fs::read(draft.client_path()).unwrap(), bytes);
    }
    #[test]
    fn backend_publishes_new_instance_from_only_cached_files() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            ..Config::default()
        };
        let root = config.root().unwrap();
        let bytes = jar("new");
        fs::create_dir_all(root.join("assets/indexes")).unwrap();
        fs::write(root.join("assets/indexes/empty.json"), br#"{"objects":{}}"#).unwrap();
        let candidate = profile(&bytes);
        let mut mutation = InstanceMutation::begin(&root, "game").unwrap();
        let draft = InstanceDraft::begin(
            &root,
            "game",
            candidate.clone(),
            &mutation,
            &TaskControl::default(),
        )
        .unwrap();
        fs::write(draft.client_path(), &bytes).unwrap();
        backend()
            .install_profile_with_session(&config, "game", candidate, &mut mutation)
            .unwrap();
        assert_eq!(crate::backend::installed(&config), vec!["game"]);
        assert_eq!(
            fs::read(root.join("versions/game/game.jar")).unwrap(),
            bytes
        );
        assert_eq!(
            crate::version::resolve(&root, "game").unwrap().meta["id"],
            "game"
        );
    }
    #[test]
    fn candidate_config_snapshot_repairs_bad_profile_without_overwriting_personal_settings() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            isolated: false,
            ..Config::default()
        };
        let root = config.root().unwrap();
        fs::create_dir_all(root.join("versions/game")).unwrap();
        fs::write(root.join("versions/game/game.json"), b"corrupt").unwrap();
        fs::write(
            root.join("versions/game/tui-mc-instance.json"),
            br#"{"display_name":"custom","settings":{"memory_mb":4096}}"#,
        )
        .unwrap();
        assert!(config.effective("game").is_err());
        let candidate = serde_json::json!({"id":"game","tuiMcIsolated":true});
        let snapshot = config.with_profile("game", &candidate);
        assert!(!snapshot.effective("other").unwrap().isolated);
        let effective = snapshot.effective("game").unwrap();
        assert!(effective.isolated);
        assert_eq!(effective.memory_mb, 4096);
        assert!(effective.effective("game").unwrap().isolated);
        assert!(!serde_json::to_string(&snapshot)
            .unwrap()
            .contains("active_profile"));
        assert_eq!(
            fs::read(root.join("versions/game/game.json")).unwrap(),
            b"corrupt"
        );
    }
    #[test]
    fn reinstall_reuses_existing_official_metadata_without_network() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            ..Config::default()
        };
        let root = config.root().unwrap();
        let bytes = jar("current");
        fs::create_dir_all(root.join("versions/game")).unwrap();
        fs::create_dir_all(root.join("assets/indexes")).unwrap();
        let metadata = serde_json::to_vec(&profile(&bytes)).unwrap();
        fs::write(root.join("versions/game/game.json"), &metadata).unwrap();
        fs::write(root.join("versions/game/game.jar"), &bytes).unwrap();
        fs::write(root.join("assets/indexes/empty.json"), br#"{"objects":{}}"#).unwrap();
        let remote = crate::metadata::RemoteVersion {
            id: "game".into(),
            kind: "release".into(),
            url: "https://must-not-request.invalid/game.json".into(),
            sha1: hash(&metadata),
        };
        backend().install(&config, &remote).unwrap();
        assert_eq!(
            fs::read(
                root.join("tui-mc-install-manifests")
                    .join(format!("{}.json", remote.sha1))
            )
            .unwrap(),
            metadata
        );
        assert_eq!(
            fs::read(root.join("versions/game/game.jar")).unwrap(),
            bytes
        );
    }
    #[test]
    fn draft_never_replaces_a_referenced_parent_client() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        let control = TaskControl::default();
        fs::create_dir_all(root.join("versions/base")).unwrap();
        fs::write(
            root.join("versions/base/base.json"),
            br#"{"id":"base","libraries":[]}"#,
        )
        .unwrap();
        let profile = serde_json::json!({"id":"game","inheritsFrom":"base","libraries":[]});
        let mut mutation = InstanceMutation::begin(root, "game").unwrap();
        let mut draft = InstanceDraft::begin(root, "game", profile, &mutation, &control).unwrap();
        assert_eq!(
            mutation.resolve_candidate(draft.profile()).unwrap().jar_id,
            "base"
        );
        let bytes = jar("parent");
        let mut requested = entry(root, &bytes);
        requested.dest = root.join("versions/base/base.jar");
        assert!(draft
            .prepare_client(
                &requested,
                |_| panic!("must not replace parent"),
                &mutation,
                &control
            )
            .is_err());
        fs::write(&requested.dest, &bytes).unwrap();
        draft
            .prepare_client(
                &requested,
                |_| panic!("must reuse verified parent"),
                &mutation,
                &control,
            )
            .unwrap();
    }
}
