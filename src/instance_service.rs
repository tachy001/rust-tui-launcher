use crate::{
    instances::InstanceLock,
    metadata::valid_id,
    version::{self, ResolvedVersion},
};
use anyhow::{bail, Result};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// Holds all instance write permissions until the complete operation returns.
pub(crate) struct InstanceMutation {
    root: PathBuf,
    target: String,
    protected: HashSet<String>,
    locks: Vec<InstanceLock>,
    recovery_messages: Vec<String>,
}
impl InstanceMutation {
    pub(crate) fn begin(root: &Path, id: &str) -> Result<Self> {
        valid_id(id)?;
        let mut session = Self {
            root: root.into(),
            target: id.into(),
            protected: HashSet::new(),
            locks: vec![],
            recovery_messages: vec![],
        };
        session.protect(id)?;
        Ok(session)
    }
    fn protect(&mut self, id: &str) -> Result<()> {
        valid_id(id)?;
        if !self.protected.contains(id) {
            let lock = InstanceLock::acquire(&self.root.join("versions").join(id))?;
            if let Some(message) =
                crate::loader_profile_service::LoaderProfileTransaction::recover(&self.root, id)?
            {
                self.recovery_messages.push(message);
            }
            if let Some(message) =
                crate::instance_recovery::InstanceRecovery::recover(&self.root, id)?
            {
                self.recovery_messages.push(message);
            }
            if crate::instance_recovery::InstanceRecovery::pending(&self.root, id)? {
                bail!(
                    "实例文件已恢复一致，但恢复记录尚未清理，暂不允许继续写入；请检查备份目录权限"
                );
            }
            self.locks.push(lock);
            self.protected.insert(id.into());
        }
        Ok(())
    }
    pub(crate) fn take_recovery_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.recovery_messages)
    }
    pub(crate) fn require(&self, root: &Path, id: &str) -> Result<()> {
        if root != self.root || id != self.target {
            bail!("实例操作会话与目标不一致");
        }
        Ok(())
    }
    pub(crate) fn require_protected(&self, root: &Path, id: &str) -> Result<()> {
        if root != self.root || !self.protected.contains(id) {
            bail!("客户端不在本次实例操作的保护范围内");
        }
        Ok(())
    }
    pub(crate) fn resolve(&mut self, id: &str) -> Result<ResolvedVersion> {
        self.resolve_using(id, None)
    }
    pub(crate) fn resolve_candidate(
        &mut self,
        profile: &serde_json::Value,
    ) -> Result<ResolvedVersion> {
        self.resolve_using(&self.target.clone(), Some(profile))
    }
    fn resolve_using(
        &mut self,
        id: &str,
        profile: Option<&serde_json::Value>,
    ) -> Result<ResolvedVersion> {
        let root = self.root.clone();
        let current =
            version::resolve_guarded(&root, id, profile, |ancestor| self.protect(ancestor))?;
        self.protect(&current.jar_id)?;
        Ok(current)
    }
}

/// Keeps published metadata and all referenced clients stable for a game run.
pub(crate) struct InstanceLaunchSession {
    root: PathBuf,
    target: String,
    protected: HashSet<String>,
    locks: Vec<InstanceLock>,
    messages: Vec<String>,
}
impl InstanceLaunchSession {
    pub(crate) fn begin(root: &Path, id: &str) -> Result<Self> {
        valid_id(id)?;
        let mut session = Self {
            root: root.into(),
            target: id.into(),
            protected: HashSet::new(),
            locks: vec![],
            messages: vec![],
        };
        session.protect(id)?;
        Ok(session)
    }
    fn protect(&mut self, id: &str) -> Result<()> {
        valid_id(id)?;
        if self.protected.contains(id) {
            return Ok(());
        }
        let directory = self.root.join("versions").join(id);
        let lock = if id == self.target {
            let lock = InstanceLock::acquire(&directory)?;
            if let Some(message) =
                crate::loader_profile_service::LoaderProfileTransaction::recover(&self.root, id)?
            {
                self.messages.push(message);
            }
            if let Some(message) =
                crate::instance_recovery::InstanceRecovery::recover(&self.root, id)?
            {
                self.messages.push(message);
            }
            lock
        } else {
            let mut lock = InstanceLock::shared(&directory)?;
            if crate::loader_profile_service::LoaderProfileTransaction::pending(&self.root, id)?
                || !crate::instance_recovery::InstanceRecovery::ready(&self.root, id)?
            {
                drop(lock);
                let exclusive = InstanceLock::acquire(&directory)?;
                if let Some(message) =
                    crate::loader_profile_service::LoaderProfileTransaction::recover(
                        &self.root, id,
                    )?
                {
                    self.messages.push(message);
                }
                if let Some(message) =
                    crate::instance_recovery::InstanceRecovery::recover(&self.root, id)?
                {
                    self.messages.push(message);
                }
                drop(exclusive);
                lock = InstanceLock::shared(&directory)?;
                if crate::loader_profile_service::LoaderProfileTransaction::pending(&self.root, id)?
                    || !crate::instance_recovery::InstanceRecovery::ready(&self.root, id)?
                {
                    bail!("父版本在恢复后再次发生变化，请重试");
                }
            }
            lock
        };
        self.locks.push(lock);
        self.protected.insert(id.into());
        Ok(())
    }
    pub(crate) fn resolve(&mut self, id: &str) -> Result<ResolvedVersion> {
        if id != self.target {
            bail!("启动会话与目标实例不一致");
        }
        let root = self.root.clone();
        let version = version::resolve_guarded(&root, id, None, |ancestor| self.protect(ancestor))?;
        self.protect(&version.jar_id)?;
        Ok(version)
    }
    pub(crate) fn messages(&self) -> &[String] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::{Backend, Message},
        config::Config,
        metadata::RemoteVersion,
    };
    use std::{fs, sync::mpsc};
    #[test]
    fn candidate_resolves_and_locks_parents_without_publishing_target() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        profile(root, "base", None);
        let candidate = serde_json::json!({"id":"child","inheritsFrom":"base","libraries":[]});
        let mut mutation = InstanceMutation::begin(root, "child").unwrap();
        let resolved = mutation.resolve_candidate(&candidate).unwrap();
        assert_eq!(resolved.chain, vec!["base", "child"]);
        assert_eq!(resolved.jar_id, "base");
        assert!(!root.join("versions/child/child.json").exists());
        mutation.require_protected(root, "base").unwrap();
        assert!(InstanceLock::shared(&root.join("versions/base")).is_err());
        assert!(mutation
            .resolve_candidate(&serde_json::json!({"id":"child","inheritsFrom":"child"}))
            .is_err());
    }
    fn profile(root: &Path, id: &str, parent: Option<&str>) {
        let dir = root.join("versions").join(id);
        fs::create_dir_all(&dir).unwrap();
        let mut value = serde_json::json!({"id":id, "libraries":[]});
        if let Some(parent) = parent {
            value["inheritsFrom"] = serde_json::json!(parent);
        }
        fs::write(dir.join(format!("{id}.json")), value.to_string()).unwrap();
    }
    fn backend() -> Backend {
        let (tx, _rx) = mpsc::channel::<Message>();
        Backend::new(tx).unwrap()
    }
    #[test]
    fn repair_and_reinstall_refuse_running_instances_before_metadata_is_written() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            ..Config::default()
        };
        let root = config.root().unwrap();
        profile(&root, "game", None);
        let path = root.join("versions/game/game.json");
        let original = fs::read(&path).unwrap();
        let _running = InstanceLock::acquire(&root.join("versions/game")).unwrap();
        let service = backend();
        assert!(service
            .repair(&config, "game")
            .unwrap_err()
            .to_string()
            .contains("实例正在"));
        let remote = RemoteVersion {
            id: "game".into(),
            kind: "release".into(),
            url: "https://must-not-be-requested.invalid/game.json".into(),
            sha1: "unused".into(),
        };
        assert!(service
            .install(&config, &remote)
            .unwrap_err()
            .to_string()
            .contains("实例正在"));
        assert_eq!(fs::read(path).unwrap(), original);
    }
    #[test]
    fn mutation_protects_inherited_clients_and_releases_all_locks() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        profile(root, "base", None);
        profile(root, "child", Some("base"));
        {
            let mut mutation = InstanceMutation::begin(root, "child").unwrap();
            assert_eq!(mutation.resolve("child").unwrap().jar_id, "base");
            assert!(InstanceLock::shared(&root.join("versions/base")).is_err());
            assert!(InstanceLock::acquire(&root.join("versions/child")).is_err());
            assert!(mutation.require(root, "different").is_err());
        }
        let _base = InstanceLock::acquire(&root.join("versions/base")).unwrap();
        let _child = InstanceLock::acquire(&root.join("versions/child")).unwrap();
    }
    #[test]
    fn running_parent_blocks_repair_and_failed_operation_releases_child() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        profile(root, "base", None);
        profile(root, "child", Some("base"));
        let _running_parent = InstanceLock::shared(&root.join("versions/base")).unwrap();
        let result = (|| -> Result<()> {
            let mut mutation = InstanceMutation::begin(root, "child")?;
            mutation.resolve("child")?;
            Ok(())
        })();
        assert!(result.is_err());
        let _child = InstanceLock::acquire(&root.join("versions/child")).unwrap();
    }
}
