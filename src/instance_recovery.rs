use crate::{metadata::valid_id, storage::atomic_write};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) const JOURNAL: &str = ".tui-mc-install-journal.json";
const DRAFTS: &str = ".tui-mc-install-drafts";
const MAX_PROFILE: u64 = 16 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stamp {
    sha1: String,
    size: u64,
}
impl Stamp {
    fn bytes(bytes: &[u8]) -> Self {
        Self {
            sha1: format!("{:x}", Sha1::digest(bytes)),
            size: bytes.len() as u64,
        }
    }
    fn read(path: &Path) -> Result<Option<Self>> {
        if !regular(path)? {
            return Ok(None);
        }
        let mut file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        let mut count = 0;
        let mut digest = Sha1::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            digest.update(&buffer[..n]);
            count += n as u64;
        }
        if count != size {
            bail!("恢复检查期间文件长度发生变化：{}", path.display());
        }
        Ok(Some(Self {
            sha1: format!("{:x}", digest.finalize()),
            size,
        }))
    }
    fn validate(&self) -> Result<()> {
        if self.sha1.len() != 40
            || !self
                .sha1
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            bail!("安装恢复记录中的文件指纹无效");
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    id: String,
    stage: String,
    old_profile: Option<Stamp>,
    new_profile: Stamp,
    old_client: Option<Stamp>,
    new_client: Stamp,
}
fn regular(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => Ok(true),
        Ok(_) => bail!("安装恢复路径必须为普通文件：{}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("无法检查恢复路径：{}", path.display())),
    }
}
fn read_small(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    if !regular(path)? {
        return Ok(None);
    }
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > limit {
        bail!("恢复文件超过读取限制：{}", path.display());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("恢复文件超过读取限制");
    }
    Ok(Some(bytes))
}

pub(crate) struct InstanceRecovery {
    directory: PathBuf,
    stage: PathBuf,
    record: Record,
}
impl InstanceRecovery {
    pub(crate) fn pending(root: &Path, id: &str) -> Result<bool> {
        valid_id(id)?;
        regular(&root.join("versions").join(id).join(JOURNAL))
    }
    pub(crate) fn prepare(root: &Path, id: &str, stage: &Path, profile: &[u8]) -> Result<Self> {
        valid_id(id)?;
        if Self::pending(root, id)? {
            bail!("存在未完成的实例提交，必须先恢复");
        }
        let directory = root.join("versions").join(id);
        let new_profile = Stamp::bytes(profile);
        if stage != directory.join(DRAFTS).join(&new_profile.sha1) {
            bail!("暂存目录与候选配置指纹不一致");
        }
        let old_profile = read_small(&directory.join(format!("{id}.json")), MAX_PROFILE)?;
        let record = Record {
            schema: 1,
            id: id.into(),
            stage: new_profile.sha1.clone(),
            old_profile: old_profile.as_deref().map(Stamp::bytes),
            new_profile,
            old_client: Stamp::read(&directory.join(format!("{id}.jar")))?,
            new_client: Stamp::read(&stage.join("client.jar"))?.context("暂存客户端不存在")?,
        };
        let transaction = Self {
            directory,
            stage: stage.into(),
            record,
        };
        transaction.validate_paths()?;
        if let Some(bytes) = &old_profile {
            atomic_write(&transaction.stage.join("profile.previous.json"), bytes)?;
        }
        atomic_write(
            &transaction.directory.join(JOURNAL),
            &serde_json::to_vec_pretty(&transaction.record)?,
        )?;
        Ok(transaction)
    }
    fn validate_paths(&self) -> Result<()> {
        if self.record.schema != 1 {
            bail!("不支持此安装恢复记录版本");
        }
        valid_id(&self.record.id)?;
        self.record.new_profile.validate()?;
        self.record.new_client.validate()?;
        if self.record.new_profile.size > MAX_PROFILE
            || self
                .record
                .old_profile
                .as_ref()
                .is_some_and(|stamp| stamp.size > MAX_PROFILE)
        {
            bail!("恢复记录中的配置大小超限");
        }
        for stamp in [&self.record.old_profile, &self.record.old_client]
            .into_iter()
            .flatten()
        {
            stamp.validate()?;
        }
        if self.record.stage != self.record.new_profile.sha1 {
            bail!("恢复记录暂存目录指纹不匹配");
        }
        for path in [self.directory.join(DRAFTS), self.stage.clone()] {
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                _ => bail!("安装恢复暂存目录缺失或不是普通目录：{}", path.display()),
            }
        }
        Ok(())
    }
    pub(crate) fn recover(root: &Path, id: &str) -> Result<Option<String>> {
        let Some(transaction) = Self::load(root, id)? else {
            return Ok(None);
        };
        let warning = transaction.finish(false)?;
        Ok(Some(match warning {
            Some(warning) => format!("已处理实例 {id} 的中断提交；{warning}"),
            None => format!("已恢复实例 {id} 的中断提交"),
        }))
    }
    fn load(root: &Path, id: &str) -> Result<Option<Self>> {
        valid_id(id)?;
        let directory = root.join("versions").join(id);
        let Some(bytes) = read_small(&directory.join(JOURNAL), 64 * 1024)? else {
            return Ok(None);
        };
        let record: Record =
            serde_json::from_slice(&bytes).context("安装恢复记录损坏，已保留现场")?;
        if record.id != id {
            bail!("安装恢复记录与实例 ID 不一致");
        }
        record.new_profile.validate()?;
        let stage = directory.join(DRAFTS).join(&record.new_profile.sha1);
        let transaction = Self {
            directory,
            stage,
            record,
        };
        transaction.validate_paths()?;
        Ok(Some(transaction))
    }
    pub(crate) fn rollback(&self) -> Result<Option<String>> {
        self.finish(true)
    }
    pub(crate) fn complete(&self) -> Result<Option<String>> {
        if Stamp::read(&self.profile_path())?.as_ref() != Some(&self.record.new_profile)
            || Stamp::read(&self.client_path())?.as_ref() != Some(&self.record.new_client)
        {
            bail!("提交后的实例文件指纹不匹配，恢复记录仍保留");
        }
        Ok(self.cleanup(true))
    }
    pub(crate) fn ready(root: &Path, id: &str) -> Result<bool> {
        let Some(transaction) = Self::load(root, id)? else {
            return Ok(true);
        };
        let profile = Stamp::read(&transaction.profile_path())?;
        let client = Stamp::read(&transaction.client_path())?;
        Ok((profile.as_ref() == Some(&transaction.record.new_profile)
            && client.as_ref() == Some(&transaction.record.new_client))
            || (profile == transaction.record.old_profile
                && client == transaction.record.old_client))
    }
    fn profile_path(&self) -> PathBuf {
        self.directory.join(format!("{}.json", self.record.id))
    }
    fn client_path(&self) -> PathBuf {
        self.directory.join(format!("{}.jar", self.record.id))
    }
    fn finish(&self, force_rollback: bool) -> Result<Option<String>> {
        let profile = Stamp::read(&self.profile_path())?;
        let client = Stamp::read(&self.client_path())?;
        if !force_rollback
            && profile.as_ref() == Some(&self.record.new_profile)
            && client.as_ref() == Some(&self.record.new_client)
        {
            return Ok(self.cleanup(true));
        }
        if profile != self.record.old_profile && profile.as_ref() != Some(&self.record.new_profile)
        {
            bail!("正式配置与恢复记录不符，可能被外部修改；恢复记录和备份已保留");
        }
        if client.is_some()
            && client != self.record.old_client
            && client.as_ref() != Some(&self.record.new_client)
        {
            bail!("客户端与恢复记录不符，可能被外部修改；未覆盖现有文件");
        }
        let backup = self.stage.join("client.previous.jar");
        let saved = Stamp::read(&backup)?;
        let old_profile_backup = self.stage.join("profile.previous.json");
        // Validate every required restoration source before the first mutation.
        if profile != self.record.old_profile
            && self.record.old_profile.is_some()
            && Stamp::read(&old_profile_backup)? != self.record.old_profile
        {
            bail!("原配置备份缺失或损坏，未执行回滚");
        }
        if client != self.record.old_client
            && self.record.old_client.is_some()
            && saved != self.record.old_client
        {
            bail!("原客户端备份缺失或损坏，未执行回滚");
        }
        if saved.is_some() && saved != self.record.old_client {
            bail!("客户端备份被修改，未执行回滚");
        }
        if client != self.record.old_client {
            if client.is_some() {
                self.preserve_new_client()?;
            }
            if self.record.old_client.is_some() {
                fs::rename(&backup, self.client_path())
                    .context("恢复原客户端失败，恢复记录仍保留")?;
            }
        }
        if profile != self.record.old_profile {
            if self.record.old_profile.is_some() {
                crate::storage::atomic_copy(
                    &self.profile_path(),
                    &mut fs::File::open(old_profile_backup)?,
                )?;
            } else {
                fs::remove_file(self.profile_path())?;
            }
        }
        if Stamp::read(&self.profile_path())? != self.record.old_profile
            || Stamp::read(&self.client_path())? != self.record.old_client
        {
            bail!("实例回滚后的文件指纹不匹配，恢复记录仍保留");
        }
        Ok(self.cleanup(false))
    }
    fn preserve_new_client(&self) -> Result<()> {
        let staged = self.stage.join("client.jar");
        match Stamp::read(&staged)? {
            None => fs::rename(self.client_path(), staged).context("无法保留客户端暂存文件")?,
            Some(stamp) if stamp == self.record.new_client => fs::remove_file(self.client_path())?,
            Some(_) => bail!("客户端暂存文件已变化，未覆盖现有暂存内容"),
        }
        Ok(())
    }
    fn cleanup(&self, committed: bool) -> Option<String> {
        let cleanup = (|| -> Result<()> {
            let mut files = vec![
                (
                    self.stage.join("client.previous.jar"),
                    self.record.old_client.as_ref(),
                ),
                (
                    self.stage.join("profile.previous.json"),
                    self.record.old_profile.as_ref(),
                ),
            ];
            if committed {
                files.push((
                    self.stage.join("profile.json"),
                    Some(&self.record.new_profile),
                ));
                files.push((self.stage.join("client.jar"), Some(&self.record.new_client)));
            }
            for (path, expected) in files {
                if let Some(actual) = Stamp::read(&path)? {
                    if Some(&actual) != expected {
                        bail!("清理时发现内容不明的备份，已保留：{}", path.display());
                    }
                    fs::remove_file(path)?;
                }
            }
            fs::remove_file(self.directory.join(JOURNAL))?;
            if committed {
                let _ = fs::remove_dir(&self.stage);
            }
            Ok(())
        })();
        cleanup
            .err()
            .map(|error| format!("实例文件已恢复一致，但清理恢复记录失败：{error:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance_service::{InstanceLaunchSession, InstanceMutation};
    use std::process::Command;
    struct Fixture {
        workspace: crate::test_support::Workspace,
        stage: PathBuf,
        old_profile: Vec<u8>,
        new_profile: Vec<u8>,
    }
    impl Fixture {
        fn new(existing: bool) -> Self {
            let workspace = crate::test_support::Workspace::new();
            let old_profile = br#"{"id":"game","marker":"old","libraries":[]}"#.to_vec();
            let new_profile = br#"{"id":"game","marker":"new","libraries":[]}"#.to_vec();
            let directory = workspace.path.join("versions/game");
            let stage = directory.join(DRAFTS).join(Stamp::bytes(&new_profile).sha1);
            fs::create_dir_all(&stage).unwrap();
            fs::write(stage.join("profile.json"), &new_profile).unwrap();
            fs::write(stage.join("client.jar"), b"new-client").unwrap();
            if existing {
                fs::write(directory.join("game.json"), &old_profile).unwrap();
                fs::write(directory.join("game.jar"), b"old-client").unwrap();
            }
            Self {
                workspace,
                stage,
                old_profile,
                new_profile,
            }
        }
        fn root(&self) -> &Path {
            &self.workspace.path
        }
        fn profile(&self) -> PathBuf {
            self.root().join("versions/game/game.json")
        }
        fn client(&self) -> PathBuf {
            self.root().join("versions/game/game.jar")
        }
        fn prepare(&self) -> InstanceRecovery {
            InstanceRecovery::prepare(self.root(), "game", &self.stage, &self.new_profile).unwrap()
        }
        fn replace_client(&self, old: bool) {
            if old {
                fs::rename(self.client(), self.stage.join("client.previous.jar")).unwrap();
            }
            fs::rename(self.stage.join("client.jar"), self.client()).unwrap();
        }
    }
    #[test]
    fn rollback_covers_each_commit_boundary_and_is_idempotent() {
        for phase in ["prepared", "backup", "client"] {
            let fixture = Fixture::new(true);
            fixture.prepare();
            if phase != "prepared" {
                fs::rename(fixture.client(), fixture.stage.join("client.previous.jar")).unwrap();
            }
            if phase == "client" {
                fs::rename(fixture.stage.join("client.jar"), fixture.client()).unwrap();
            }
            assert!(InstanceRecovery::recover(fixture.root(), "game")
                .unwrap()
                .is_some());
            assert_eq!(fs::read(fixture.profile()).unwrap(), fixture.old_profile);
            assert_eq!(fs::read(fixture.client()).unwrap(), b"old-client");
            assert_eq!(
                fs::read(fixture.stage.join("client.jar")).unwrap(),
                b"new-client"
            );
            assert!(InstanceRecovery::recover(fixture.root(), "game")
                .unwrap()
                .is_none());
        }
    }
    #[test]
    fn published_pair_finishes_even_if_cleanup_was_interrupted() {
        let fixture = Fixture::new(true);
        fixture.prepare();
        fixture.replace_client(true);
        atomic_write(&fixture.profile(), &fixture.new_profile).unwrap();
        fs::remove_file(fixture.stage.join("profile.previous.json")).unwrap();
        assert!(InstanceRecovery::ready(fixture.root(), "game").unwrap());
        InstanceRecovery::recover(fixture.root(), "game").unwrap();
        assert_eq!(fs::read(fixture.profile()).unwrap(), fixture.new_profile);
        assert_eq!(fs::read(fixture.client()).unwrap(), b"new-client");
        assert!(!InstanceRecovery::pending(fixture.root(), "game").unwrap());
    }
    #[test]
    fn interrupted_new_install_returns_to_absent_published_files() {
        for published in [false, true] {
            let fixture = Fixture::new(false);
            let transaction = fixture.prepare();
            fixture.replace_client(false);
            if published {
                atomic_write(&fixture.profile(), &fixture.new_profile).unwrap();
            }
            transaction.rollback().unwrap();
            assert!(!fixture.profile().exists());
            assert!(!fixture.client().exists());
            assert_eq!(
                fs::read(fixture.stage.join("client.jar")).unwrap(),
                b"new-client"
            );
        }
    }
    #[test]
    fn recovery_resumes_after_client_restoration_before_profile_restoration() {
        let fixture = Fixture::new(true);
        fixture.prepare();
        fixture.replace_client(true);
        atomic_write(&fixture.profile(), &fixture.new_profile).unwrap();
        fs::rename(fixture.client(), fixture.stage.join("client.jar")).unwrap();
        fs::rename(fixture.stage.join("client.previous.jar"), fixture.client()).unwrap();
        InstanceRecovery::recover(fixture.root(), "game").unwrap();
        assert_eq!(fs::read(fixture.profile()).unwrap(), fixture.old_profile);
        assert_eq!(fs::read(fixture.client()).unwrap(), b"old-client");
        assert_eq!(
            fs::read(fixture.stage.join("client.jar")).unwrap(),
            b"new-client"
        );
        assert!(!InstanceRecovery::pending(fixture.root(), "game").unwrap());
    }
    #[test]
    fn unknown_external_edits_are_preserved_and_block_launch() {
        for edit in ["profile", "client", "backup"] {
            let fixture = Fixture::new(true);
            fixture.prepare();
            fixture.replace_client(true);
            let path = match edit {
                "profile" => fixture.profile(),
                "client" => fixture.client(),
                _ => fixture.stage.join("client.previous.jar"),
            };
            fs::write(&path, b"external-change").unwrap();
            assert!(InstanceRecovery::recover(fixture.root(), "game").is_err());
            assert!(InstanceLaunchSession::begin(fixture.root(), "game").is_err());
            assert_eq!(fs::read(&path).unwrap(), b"external-change");
            assert!(InstanceRecovery::pending(fixture.root(), "game").unwrap());
        }
    }
    #[test]
    fn malformed_or_wrong_identity_journals_never_modify_active_files() {
        for kind in ["bad-json", "wrong-id", "unsafe-stage", "bad-hash"] {
            let fixture = Fixture::new(true);
            fixture.prepare();
            let path = fixture.root().join("versions/game").join(JOURNAL);
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            match kind {
                "wrong-id" => value["id"] = "other".into(),
                "unsafe-stage" => value["stage"] = "../escape".into(),
                "bad-hash" => value["new_profile"]["sha1"] = "../escape".into(),
                _ => {}
            }
            fs::write(
                &path,
                if kind == "bad-json" {
                    b"{".to_vec()
                } else {
                    serde_json::to_vec(&value).unwrap()
                },
            )
            .unwrap();
            assert!(InstanceRecovery::recover(fixture.root(), "game").is_err());
            assert_eq!(fs::read(fixture.profile()).unwrap(), fixture.old_profile);
            assert_eq!(fs::read(fixture.client()).unwrap(), b"old-client");
        }
    }
    #[test]
    fn inherited_version_is_recovered_before_its_metadata_is_parsed() {
        let fixture = Fixture::new(true);
        fixture.prepare();
        fixture.replace_client(true);
        let child = fixture.root().join("versions/child");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            child.join("child.json"),
            br#"{"id":"child","inheritsFrom":"game"}"#,
        )
        .unwrap();
        let mut launch = InstanceLaunchSession::begin(fixture.root(), "child").unwrap();
        let version = launch.resolve("child").unwrap();
        assert_eq!(version.meta["marker"], "old");
        assert!(!launch.messages().is_empty());
        assert_eq!(fs::read(fixture.client()).unwrap(), b"old-client");
        assert!(InstanceMutation::begin(fixture.root(), "game").is_err());
        drop(launch);
        assert!(InstanceMutation::begin(fixture.root(), "game").is_ok());
    }
    #[test]
    fn explicit_jar_owner_is_locked_for_entire_launch_session() {
        let fixture = Fixture::new(true);
        let child = fixture.root().join("versions/child");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            child.join("child.json"),
            br#"{"id":"child","jar":"game","libraries":[]}"#,
        )
        .unwrap();
        let mut launch = InstanceLaunchSession::begin(fixture.root(), "child").unwrap();
        assert_eq!(launch.resolve("child").unwrap().jar_id, "game");
        assert!(InstanceMutation::begin(fixture.root(), "game").is_err());
        drop(launch);
        assert!(InstanceMutation::begin(fixture.root(), "game").is_ok());
    }
    #[test]
    fn process_exit_leaves_recoverable_journal_and_releases_instance_lock() {
        for phase in ["backup", "client", "profile"] {
            let fixture = Fixture::new(true);
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "instance_recovery::tests::crash_helper",
                    "--ignored",
                ])
                .env("TUI_INSTALL_RECOVERY_TEST_ROOT", fixture.root())
                .env("TUI_INSTALL_RECOVERY_TEST_PHASE", phase)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{phase}");
            let mut mutation = InstanceMutation::begin(fixture.root(), "game").unwrap();
            assert!(!mutation.take_recovery_messages().is_empty());
            let expected = if phase == "profile" {
                &fixture.new_profile
            } else {
                &fixture.old_profile
            };
            assert_eq!(fs::read(fixture.profile()).unwrap(), *expected);
            assert_eq!(
                fs::read(fixture.client()).unwrap(),
                if phase == "profile" {
                    b"new-client"
                } else {
                    b"old-client"
                }
            );
            assert!(!InstanceRecovery::pending(fixture.root(), "game").unwrap());
        }
    }
    #[test]
    #[ignore = "spawned only by process_exit_leaves_recoverable_journal_and_releases_instance_lock"]
    fn crash_helper() {
        let root = PathBuf::from(
            std::env::var_os("TUI_INSTALL_RECOVERY_TEST_ROOT").expect("parent fixture required"),
        );
        let root = fs::canonicalize(root).unwrap();
        let temporary = fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(
            root.starts_with(&temporary)
                && root != temporary
                && root
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("tui-mc-test-")
        );
        let phase = std::env::var("TUI_INSTALL_RECOVERY_TEST_PHASE").unwrap();
        assert!(["backup", "client", "profile"].contains(&phase.as_str()));
        let _mutation = InstanceMutation::begin(&root, "game").unwrap();
        let profile = br#"{"id":"game","marker":"new","libraries":[]}"#;
        let stage = root
            .join("versions/game")
            .join(DRAFTS)
            .join(Stamp::bytes(profile).sha1);
        InstanceRecovery::prepare(&root, "game", &stage, profile).unwrap();
        fs::rename(
            root.join("versions/game/game.jar"),
            stage.join("client.previous.jar"),
        )
        .unwrap();
        if phase != "backup" {
            fs::rename(
                stage.join("client.jar"),
                root.join("versions/game/game.jar"),
            )
            .unwrap();
        }
        if phase == "profile" {
            atomic_write(&root.join("versions/game/game.json"), profile).unwrap();
        }
        // Bypass Rust destructors, as when a launcher process terminates abruptly.
        std::process::exit(0);
    }
}
