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
}
impl InstanceMutation {
    pub(crate) fn begin(root: &Path, id: &str) -> Result<Self> {
        valid_id(id)?;
        let mut session = Self {
            root: root.into(),
            target: id.into(),
            protected: HashSet::new(),
            locks: vec![],
        };
        session.protect(id)?;
        Ok(session)
    }
    fn protect(&mut self, id: &str) -> Result<()> {
        valid_id(id)?;
        if !self.protected.contains(id) {
            let lock = InstanceLock::acquire(&self.root.join("versions").join(id))?;
            self.locks.push(lock);
            self.protected.insert(id.into());
        }
        Ok(())
    }
    pub(crate) fn require(&self, root: &Path, id: &str) -> Result<()> {
        if root != self.root || id != self.target {
            bail!("实例操作会话与目标不一致");
        }
        Ok(())
    }
    pub(crate) fn resolve(&mut self, id: &str) -> Result<ResolvedVersion> {
        self.protect(id)?;
        let initial = version::resolve(&self.root, id)?;
        for ancestor in &initial.chain {
            self.protect(ancestor)?;
        }
        self.protect(&initial.jar_id)?;
        // Re-read after acquiring parents: another process could have updated them meanwhile.
        let current = version::resolve(&self.root, id)?;
        if current.chain != initial.chain || current.jar_id != initial.jar_id {
            bail!("版本依赖在获取操作锁期间发生变化，请重试");
        }
        Ok(current)
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
