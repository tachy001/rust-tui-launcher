use crate::accounts::{Account, AccountKind, Accounts};
use anyhow::{bail, Context, Result};
use std::{fs, path::Path};
use zeroize::Zeroizing;

pub(crate) trait CredentialStore {
    fn read(&self, account: &Account, kind: &str) -> Result<Option<Zeroizing<String>>>;
    fn write(&self, account: &Account, kind: &str, value: &str) -> Result<()>;
    fn delete(&self, account: &Account, kind: &str) -> Result<()>;
}
pub(crate) struct SystemCredentials;
impl SystemCredentials {
    fn entry(account: &Account, kind: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(
            "tui-mc",
            &format!("{}:{}:{kind}", account.id, account.client_id),
        )
        .context("无法打开系统安全凭证存储")
    }
}
impl CredentialStore for SystemCredentials {
    fn read(&self, account: &Account, kind: &str) -> Result<Option<Zeroizing<String>>> {
        match Self::entry(account, kind)?.get_password() {
            Ok(value) => Ok(Some(Zeroizing::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("读取账户凭证失败"),
        }
    }
    fn write(&self, account: &Account, kind: &str, value: &str) -> Result<()> {
        Self::entry(account, kind)?
            .set_password(value)
            .context("安全凭证保存失败；令牌不会写入普通配置文件")
    }
    fn delete(&self, account: &Account, kind: &str) -> Result<()> {
        match Self::entry(account, kind)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("删除系统凭证失败"),
        }
    }
}
pub(crate) trait AccountMutationLease {}
struct AccountIndexLock {
    _file: fs::File,
}
impl AccountMutationLease for AccountIndexLock {}
impl AccountIndexLock {
    fn acquire(path: &Path) -> Result<Self> {
        fs::create_dir_all(path.parent().context("账户锁缺少父目录")?)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .context("无法打开账户索引锁")?;
        file.try_lock().context("账户正被其他操作修改，稍后重试")?;
        Ok(Self { _file: file })
    }
}
pub(crate) trait AccountIndexStore {
    /// Hold through index reads, secret changes, save and any rollback.
    fn lock(&self) -> Result<Box<dyn AccountMutationLease + '_>>;
    fn lock_refresh(&self, id: &str) -> Result<Box<dyn AccountMutationLease + '_>>;
    fn load(&self) -> Result<Accounts>;
    /// A failed save must leave the previous index intact.
    fn save(&self, accounts: &Accounts) -> Result<()>;
}
pub(crate) struct FileAccountIndex;
impl AccountIndexStore for FileAccountIndex {
    fn lock_refresh(&self, id: &str) -> Result<Box<dyn AccountMutationLease + '_>> {
        use sha1::{Digest, Sha1};
        let name = format!("{:x}.lock", Sha1::digest(id.as_bytes()));
        Ok(Box::new(
            AccountIndexLock::acquire(&Path::new(".tui-mc/account-refresh-locks").join(name))
                .context("该账户已有刷新请求，请稍后重试")?,
        ))
    }
    fn lock(&self) -> Result<Box<dyn AccountMutationLease + '_>> {
        Ok(Box::new(AccountIndexLock::acquire(Path::new(
            ".tui-mc/accounts.lock",
        ))?))
    }
    fn load(&self) -> Result<Accounts> {
        Accounts::load()
    }
    fn save(&self, accounts: &Accounts) -> Result<()> {
        accounts.save()
    }
}

/// Coordinates account metadata with secret storage without exposing tokens to the UI.
pub(crate) struct AccountService<'a> {
    index: &'a dyn AccountIndexStore,
    credentials: &'a dyn CredentialStore,
}
impl<'a> AccountService<'a> {
    pub(crate) fn new(
        index: &'a dyn AccountIndexStore,
        credentials: &'a dyn CredentialStore,
    ) -> Self {
        Self { index, credentials }
    }
    fn restore(&self, account: &Account, kind: &str, previous: Option<&str>) -> Result<()> {
        match previous {
            Some(value) => self.credentials.write(account, kind, value),
            None => self.credentials.delete(account, kind),
        }
    }
    pub(crate) fn select(&self, id: &str) -> Result<Accounts> {
        let _lease = self.index.lock()?;
        let mut accounts = self.index.load()?;
        if !accounts.entries.iter().any(|account| account.id == id) {
            bail!("账户不存在");
        }
        accounts.active = Some(id.into());
        self.index.save(&accounts)?;
        Ok(accounts)
    }
    pub(crate) fn add_offline(&self, name: &str) -> Result<Accounts> {
        let config = crate::config::Config {
            username: name.into(),
            ..crate::config::Config::default()
        };
        config.validate()?;
        let _lease = self.index.lock()?;
        let mut accounts = self.index.load()?;
        accounts.upsert(Account {
            id: format!("offline:{}", crate::backend::offline_uuid(name)),
            name: name.into(),
            kind: AccountKind::Offline,
            client_id: String::new(),
            expires_at: 0,
            xuid: String::new(),
            generation: 0,
        });
        self.index.save(&accounts)?;
        Ok(accounts)
    }
    pub(crate) fn commit_login(
        &self,
        account: Account,
        refresh: &str,
        minecraft: &str,
    ) -> Result<()> {
        self.commit_result(account, refresh, minecraft, None)
    }
    fn commit_result(
        &self,
        mut account: Account,
        refresh: &str,
        minecraft: &str,
        expected: Option<&Account>,
    ) -> Result<()> {
        if account.kind != AccountKind::Microsoft || refresh.is_empty() || minecraft.is_empty() {
            bail!("正版登录结果缺少有效账户或凭证");
        }
        let _lease = self.index.lock()?;
        let mut accounts = self.index.load()?;
        if let Some(expected) = expected {
            if account.id != expected.id || account.client_id != expected.client_id {
                bail!("刷新后的账户身份或客户端不一致，拒绝保存");
            }
            Self::require_refresh_account(&accounts, expected)?;
            if !self
                .credentials
                .read(expected, "refresh")?
                .is_some_and(|stored| stored.as_str() == refresh)
            {
                bail!("账户凭证已被其他登录修改，当前刷新结果已过期，请重试");
            }
        }
        let next_revision = accounts
            .entries
            .iter()
            .map(|entry| entry.generation)
            .max()
            .unwrap_or(0)
            .max(accounts.revision)
            .checked_add(1)
            .context("账户修订号已耗尽，拒绝覆盖账户状态")?;
        account.generation = next_revision;
        let previous_active = accounts.active.clone();
        let previous_access = self.credentials.read(&account, "minecraft")?;
        // Keep the newest refresh token durable even when a later stage fails.
        self.credentials.write(&account, "refresh", refresh)?;
        let result = (|| -> Result<()> {
            self.credentials.write(&account, "minecraft", minecraft)?;
            accounts.upsert(account.clone());
            accounts.revision = next_revision;
            if expected.is_some() {
                accounts.active = previous_active;
            }
            self.index.save(&accounts)
        })();
        if let Err(error) = result {
            if self
                .restore(
                    &account,
                    "minecraft",
                    previous_access.as_ref().map(|value| value.as_str()),
                )
                .is_err()
            {
                return Err(error).context(
                    "登录保存失败，旧游戏凭证恢复失败，请重新登录；刷新凭证不会写入普通配置",
                );
            }
            return Err(error).context("登录保存失败，原账号索引与游戏凭证已保留");
        }
        Ok(())
    }
    pub(crate) fn remove(&self, id: &str) -> Result<Accounts> {
        let _lease = self.index.lock()?;
        let mut accounts = self.index.load()?;
        let account = accounts
            .entries
            .iter()
            .find(|account| account.id == id)
            .context("账户不存在")?
            .clone();
        let previous = if account.kind == AccountKind::Microsoft {
            vec![
                ("refresh", self.credentials.read(&account, "refresh")?),
                ("minecraft", self.credentials.read(&account, "minecraft")?),
            ]
        } else {
            vec![]
        };
        let result = (|| -> Result<()> {
            for (kind, _) in &previous {
                self.credentials.delete(&account, kind)?;
            }
            accounts.entries.retain(|account| account.id != id);
            if accounts.active.as_deref() == Some(id) {
                accounts.active = accounts.entries.first().map(|account| account.id.clone());
            }
            self.index.save(&accounts)
        })();
        if let Err(error) = result {
            let mut failed = false;
            for (kind, secret) in &previous {
                failed |= self
                    .restore(&account, kind, secret.as_ref().map(|value| value.as_str()))
                    .is_err();
            }
            return Err(error).context(if failed {
                "账户移除失败，部分安全凭证无法恢复，请重新登录"
            } else {
                "账户移除失败，原账号与凭证已保留"
            });
        }
        Ok(accounts)
    }
    fn require_refresh_account(accounts: &Accounts, expected: &Account) -> Result<()> {
        let current = accounts
            .entries
            .iter()
            .find(|account| account.id == expected.id)
            .context("刷新期间账户已被移除，请重新登录")?;
        if current.kind != AccountKind::Microsoft || current.client_id != expected.client_id {
            bail!("刷新期间账户的登录配置已改变，请重新登录");
        }
        if current.generation != expected.generation {
            bail!("账户已重新登录或完成其他刷新，当前刷新结果已过期，请重试");
        }
        Ok(())
    }
    pub(crate) fn begin_refresh(&self, account: &Account) -> Result<RefreshSession<'a>> {
        if account.kind != AccountKind::Microsoft {
            bail!("仅正版账户支持令牌刷新");
        }
        let lease = self.index.lock_refresh(&account.id)?;
        let _lease = self.index.lock()?;
        let accounts = self.index.load()?;
        Self::require_refresh_account(&accounts, account)?;
        let token = self
            .credentials
            .read(account, "refresh")?
            .filter(|token| !token.is_empty())
            .context("读取账户凭证失败，请重新登录")?;
        Ok(RefreshSession {
            service: AccountService::new(self.index, self.credentials),
            account: account.clone(),
            token,
            saved: false,
            _lease: lease,
        })
    }
}

/// Holds only this account's refresh lock across OAuth/Xbox/Minecraft requests.
pub(crate) struct RefreshSession<'a> {
    service: AccountService<'a>,
    account: Account,
    token: Zeroizing<String>,
    saved: bool,
    _lease: Box<dyn AccountMutationLease + 'a>,
}
impl RefreshSession<'_> {
    pub(crate) fn account(&self) -> &Account {
        &self.account
    }
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
    pub(crate) fn save_refresh(&mut self, token: &str) -> Result<()> {
        if token.is_empty() {
            bail!("刷新结果缺少有效凭证");
        }
        let _lease = self.service.index.lock()?;
        let accounts = self.service.index.load()?;
        AccountService::require_refresh_account(&accounts, &self.account)?;
        if !self
            .service
            .credentials
            .read(&self.account, "refresh")?
            .is_some_and(|stored| stored.as_str() == self.token.as_str())
        {
            bail!("账户凭证已被其他登录修改，当前刷新结果已过期，请重试");
        }
        self.service
            .credentials
            .write(&self.account, "refresh", token)?;
        self.token = Zeroizing::new(token.into());
        self.saved = true;
        Ok(())
    }
    pub(crate) fn commit(&self, account: Account, minecraft: &str) -> Result<()> {
        if !self.saved {
            bail!("尚未保存本次 OAuth 刷新结果，拒绝提交游戏凭证");
        }
        self.service
            .commit_result(account, &self.token, minecraft, Some(&self.account))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
    };
    struct Index {
        accounts: RefCell<Accounts>,
        fail: Cell<bool>,
        locked: Cell<bool>,
        deny_lock: Cell<bool>,
        refresh_locked: RefCell<std::collections::HashSet<String>>,
    }
    struct MemoryLease<'a>(&'a Cell<bool>);
    impl AccountMutationLease for MemoryLease<'_> {}
    impl Drop for MemoryLease<'_> {
        fn drop(&mut self) {
            self.0.set(false);
        }
    }
    struct MemoryRefreshLease<'a> {
        held: &'a RefCell<std::collections::HashSet<String>>,
        id: String,
    }
    impl AccountMutationLease for MemoryRefreshLease<'_> {}
    impl Drop for MemoryRefreshLease<'_> {
        fn drop(&mut self) {
            self.held.borrow_mut().remove(&self.id);
        }
    }
    impl AccountIndexStore for Index {
        fn lock_refresh(&self, id: &str) -> Result<Box<dyn AccountMutationLease + '_>> {
            if !self.refresh_locked.borrow_mut().insert(id.into()) {
                bail!("该账户已有刷新请求");
            }
            Ok(Box::new(MemoryRefreshLease {
                held: &self.refresh_locked,
                id: id.into(),
            }))
        }
        fn lock(&self) -> Result<Box<dyn AccountMutationLease + '_>> {
            if self.deny_lock.get() || self.locked.replace(true) {
                bail!("索引锁不可用");
            }
            Ok(Box::new(MemoryLease(&self.locked)))
        }
        fn load(&self) -> Result<Accounts> {
            assert!(
                self.locked.get(),
                "read-modify-save must acquire the lease before reading"
            );
            Ok(self.accounts.borrow().clone())
        }
        fn save(&self, accounts: &Accounts) -> Result<()> {
            assert!(self.locked.get(), "save must remain inside the lease");
            if self.fail.replace(false) {
                bail!("索引保存失败");
            }
            *self.accounts.borrow_mut() = accounts.clone();
            Ok(())
        }
    }
    #[derive(Default)]
    struct Credentials {
        values: RefCell<HashMap<String, Zeroizing<String>>>,
        fail_write: RefCell<Option<String>>,
        fail_delete: RefCell<Option<String>>,
    }
    impl CredentialStore for Credentials {
        fn read(&self, account: &Account, kind: &str) -> Result<Option<Zeroizing<String>>> {
            Ok(self
                .values
                .borrow()
                .get(&format!("{}:{}:{kind}", account.id, account.client_id))
                .cloned())
        }
        fn write(&self, account: &Account, kind: &str, value: &str) -> Result<()> {
            if self.fail_write.borrow().as_deref() == Some(kind) {
                self.fail_write.borrow_mut().take();
                bail!("凭证保存失败");
            }
            self.values.borrow_mut().insert(
                format!("{}:{}:{kind}", account.id, account.client_id),
                Zeroizing::new(value.into()),
            );
            Ok(())
        }
        fn delete(&self, account: &Account, kind: &str) -> Result<()> {
            if self.fail_delete.borrow().as_deref() == Some(kind) {
                self.fail_delete.borrow_mut().take();
                bail!("凭证删除失败");
            }
            self.values
                .borrow_mut()
                .remove(&format!("{}:{}:{kind}", account.id, account.client_id));
            Ok(())
        }
    }
    fn account() -> Account {
        Account {
            id: "microsoft:123".into(),
            name: "Old".into(),
            kind: AccountKind::Microsoft,
            client_id: "client".into(),
            expires_at: 10,
            xuid: String::new(),
            generation: 0,
        }
    }
    fn fixture() -> (Index, Credentials) {
        let mut accounts = Accounts::default();
        accounts.upsert(account());
        let credentials = Credentials::default();
        credentials
            .write(&account(), "refresh", "old-refresh")
            .unwrap();
        credentials
            .write(&account(), "minecraft", "old-access")
            .unwrap();
        (
            Index {
                accounts: RefCell::new(accounts),
                fail: Cell::new(false),
                locked: Cell::new(false),
                deny_lock: Cell::new(false),
                refresh_locked: RefCell::default(),
            },
            credentials,
        )
    }
    fn assert_original(index: &Index, credentials: &Credentials) {
        assert_eq!(
            index.accounts.borrow().active.as_deref(),
            Some("microsoft:123")
        );
        assert_eq!(index.accounts.borrow().entries[0].name, "Old");
        assert_eq!(
            credentials
                .read(&account(), "minecraft")
                .unwrap()
                .unwrap()
                .as_str(),
            "old-access"
        );
    }
    #[test]
    fn failed_login_preserves_old_access_and_the_newest_refresh_token() {
        for fail_index in [false, true] {
            let (index, credentials) = fixture();
            index.fail.set(fail_index);
            if !fail_index {
                *credentials.fail_write.borrow_mut() = Some("minecraft".into());
            }
            let mut next = account();
            next.name = "New".into();
            next.expires_at = 100;
            assert!(AccountService::new(&index, &credentials)
                .commit_result(next, "new-refresh", "new-access", None)
                .is_err());
            assert_original(&index, &credentials);
            assert_eq!(
                credentials
                    .read(&account(), "refresh")
                    .unwrap()
                    .unwrap()
                    .as_str(),
                "new-refresh"
            );
        }
    }
    #[test]
    fn remove_restores_credentials_after_second_delete_or_index_save_failure() {
        for fail_index in [false, true] {
            let (index, credentials) = fixture();
            index.fail.set(fail_index);
            if !fail_index {
                *credentials.fail_delete.borrow_mut() = Some("minecraft".into());
            }
            assert!(AccountService::new(&index, &credentials)
                .remove(&account().id)
                .is_err());
            assert_original(&index, &credentials);
            assert_eq!(
                credentials
                    .read(&account(), "refresh")
                    .unwrap()
                    .unwrap()
                    .as_str(),
                "old-refresh"
            );
        }
    }
    #[test]
    fn refreshing_an_inactive_account_does_not_change_the_selection() {
        let (index, credentials) = fixture();
        index.accounts.borrow_mut().active = None;
        let service = AccountService::new(&index, &credentials);
        let mut session = service.begin_refresh(&account()).unwrap();
        session.save_refresh("new-refresh").unwrap();
        session.commit(account(), "new-access").unwrap();
        drop(session);
        assert!(index.accounts.borrow().active.is_none());
        index.accounts.borrow_mut().entries.clear();
        assert!(service.begin_refresh(&account()).is_err());
    }
    #[test]
    fn failed_select_or_offline_add_does_not_publish_a_new_index() {
        let (index, credentials) = fixture();
        index.fail.set(true);
        assert!(AccountService::new(&index, &credentials)
            .add_offline("Player")
            .is_err());
        assert_eq!(index.accounts.borrow().entries.len(), 1);
        let mut other = account();
        other.id = "offline:other".into();
        other.kind = AccountKind::Offline;
        index.accounts.borrow_mut().entries.push(other);
        index.fail.set(true);
        assert!(AccountService::new(&index, &credentials)
            .select("offline:other")
            .is_err());
        assert_original(&index, &credentials);
    }
    #[test]
    fn failed_refresh_write_keeps_existing_account_and_credentials() {
        let (index, credentials) = fixture();
        *credentials.fail_write.borrow_mut() = Some("refresh".into());
        let mut session = AccountService::new(&index, &credentials)
            .begin_refresh(&account())
            .unwrap();
        assert!(session.save_refresh("new-refresh").is_err());
        assert_original(&index, &credentials);
        assert_eq!(
            credentials
                .read(&account(), "refresh")
                .unwrap()
                .unwrap()
                .as_str(),
            "old-refresh"
        );
    }
    #[test]
    fn failed_credential_restore_reports_relogin_and_still_restores_other_secret() {
        let (index, credentials) = fixture();
        index.fail.set(true);
        *credentials.fail_write.borrow_mut() = Some("refresh".into());
        let error = AccountService::new(&index, &credentials)
            .remove(&account().id)
            .err()
            .unwrap();
        assert!(error
            .to_string()
            .contains("部分安全凭证无法恢复，请重新登录"));
        assert_original(&index, &credentials);
        assert!(credentials.read(&account(), "refresh").unwrap().is_none());
        let message = format!("{error:#}");
        assert!(!message.contains("old-refresh"));
        assert!(!message.contains("old-access"));
    }
    #[test]
    fn successful_remove_deletes_both_secrets_and_selects_the_remaining_account() {
        let (index, credentials) = fixture();
        let mut offline = account();
        offline.id = "offline:1".into();
        offline.kind = AccountKind::Offline;
        index.accounts.borrow_mut().entries.push(offline);
        let updated = AccountService::new(&index, &credentials)
            .remove(&account().id)
            .unwrap();
        assert_eq!(updated.active.as_deref(), Some("offline:1"));
        assert!(credentials.values.borrow().is_empty());
        let text = serde_json::to_string(&updated).unwrap();
        assert!(!text.contains("old-refresh"));
        assert!(!text.contains("old-access"));
    }
    #[test]
    fn file_lease_blocks_overlapping_mutations_and_is_released_on_drop() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("accounts.lock");
        let lease = AccountIndexLock::acquire(&path).unwrap();
        assert!(AccountIndexLock::acquire(&path).is_err());
        drop(lease);
        let _next = AccountIndexLock::acquire(&path).unwrap();
        assert!(
            path.is_file(),
            "lock file must remain to prevent unlink races"
        );
    }
    #[test]
    fn lock_failure_prevents_all_index_and_credential_mutations() {
        let (index, credentials) = fixture();
        index.deny_lock.set(true);
        let service = AccountService::new(&index, &credentials);
        assert!(service.select(&account().id).is_err());
        assert!(service.add_offline("Player").is_err());
        assert!(service.remove(&account().id).is_err());
        assert!(service.commit_login(account(), "new", "access").is_err());
        assert!(service
            .begin_refresh(&account())
            .and_then(|mut session| session.save_refresh("new"))
            .is_err());
        assert_original(&index, &credentials);
        assert_eq!(
            credentials
                .read(&account(), "refresh")
                .unwrap()
                .unwrap()
                .as_str(),
            "old-refresh"
        );
        assert!(!index.locked.get());
    }
    #[test]
    fn removed_or_reconfigured_accounts_cannot_receive_stale_refresh_results() {
        let (index, credentials) = fixture();
        let service = AccountService::new(&index, &credentials);
        index.accounts.borrow_mut().entries[0].client_id = "different-client".into();
        assert!(service
            .begin_refresh(&account())
            .and_then(|mut session| session.save_refresh("new"))
            .is_err());
        assert!(service
            .commit_result(account(), "new", "access", Some(&account()))
            .is_err());
        index.accounts.borrow_mut().entries.clear();
        assert!(service
            .begin_refresh(&account())
            .and_then(|mut session| session.save_refresh("new"))
            .is_err());
        assert_eq!(
            credentials
                .read(&account(), "refresh")
                .unwrap()
                .unwrap()
                .as_str(),
            "old-refresh"
        );
        assert!(!index.locked.get());
    }
    #[test]
    fn early_refresh_save_keeps_current_selection_and_index_metadata() {
        let (index, credentials) = fixture();
        index.accounts.borrow_mut().active = None;
        AccountService::new(&index, &credentials)
            .begin_refresh(&account())
            .and_then(|mut session| session.save_refresh("new-refresh"))
            .unwrap();
        assert!(index.accounts.borrow().active.is_none());
        assert_eq!(index.accounts.borrow().entries[0].expires_at, 10);
        assert_eq!(
            credentials
                .read(&account(), "refresh")
                .unwrap()
                .unwrap()
                .as_str(),
            "new-refresh"
        );
        assert!(!index.locked.get());
    }
    #[test]
    fn refresh_sessions_serialize_one_account_without_blocking_selection_or_other_accounts() {
        let (index, credentials) = fixture();
        let mut other = account();
        other.id = "microsoft:456".into();
        credentials
            .write(&other, "refresh", "other-original")
            .unwrap();
        index.accounts.borrow_mut().entries.push(other.clone());
        let service = AccountService::new(&index, &credentials);
        let mut session = service.begin_refresh(&account()).unwrap();
        assert!(service.begin_refresh(&account()).is_err());
        let mut other_session = service.begin_refresh(&other).unwrap();
        service.select(&other.id).unwrap();
        assert!(!index.locked.get());
        session.save_refresh("rotated").unwrap();
        session.commit(account(), "access").unwrap();
        assert_eq!(
            index.accounts.borrow().active.as_deref(),
            Some(other.id.as_str())
        );
        assert_eq!(index.accounts.borrow().revision, 1);
        assert!(session.commit(account(), "duplicate-access").is_err());
        other_session.save_refresh("other-refresh").unwrap();
        other_session.commit(other.clone(), "other-access").unwrap();
        assert_eq!(index.accounts.borrow().revision, 2);
        drop(session);
        let latest = index
            .accounts
            .borrow()
            .entries
            .iter()
            .find(|entry| entry.id == account().id)
            .unwrap()
            .clone();
        let _next = service.begin_refresh(&latest).unwrap();
    }
    #[test]
    fn same_client_relogin_invalidates_old_oauth_response_even_with_identical_expiry() {
        let (index, credentials) = fixture();
        let service = AccountService::new(&index, &credentials);
        let mut session = service.begin_refresh(&account()).unwrap();
        service
            .commit_login(account(), "interactive-refresh", "interactive-access")
            .unwrap();
        assert!(session.save_refresh("late-refresh").is_err());
        assert_eq!(
            index.accounts.borrow().entries[0].expires_at,
            account().expires_at
        );
        assert_eq!(index.accounts.borrow().entries[0].generation, 1);
        assert_eq!(
            credentials
                .read(&account(), "refresh")
                .unwrap()
                .unwrap()
                .as_str(),
            "interactive-refresh"
        );
        assert_eq!(
            credentials
                .read(&account(), "minecraft")
                .unwrap()
                .unwrap()
                .as_str(),
            "interactive-access"
        );
        drop(session);
        assert!(service.begin_refresh(&account()).is_err());
        let latest = index.accounts.borrow().entries[0].clone();
        let _retry = service.begin_refresh(&latest).unwrap();
    }
    #[test]
    fn reauthentication_after_oauth_save_rejects_the_late_minecraft_commit() {
        let (index, credentials) = fixture();
        let service = AccountService::new(&index, &credentials);
        let mut session = service.begin_refresh(&account()).unwrap();
        session.save_refresh("rotated-before-login").unwrap();
        service
            .commit_login(account(), "interactive-refresh", "interactive-access")
            .unwrap();
        assert!(session.commit(account(), "late-minecraft-access").is_err());
        assert_eq!(
            credentials
                .read(&account(), "refresh")
                .unwrap()
                .unwrap()
                .as_str(),
            "interactive-refresh"
        );
        assert_eq!(
            credentials
                .read(&account(), "minecraft")
                .unwrap()
                .unwrap()
                .as_str(),
            "interactive-access"
        );
    }
    #[test]
    fn remove_and_readd_does_not_reuse_the_previous_account_generation() {
        let (index, credentials) = fixture();
        let service = AccountService::new(&index, &credentials);
        service
            .commit_login(account(), "first-refresh", "first-access")
            .unwrap();
        let first = index.accounts.borrow().entries[0].clone();
        let mut session = service.begin_refresh(&first).unwrap();
        session.save_refresh("old-rotation").unwrap();
        service.remove(&first.id).unwrap();
        assert_eq!(index.accounts.borrow().revision, 1);
        service
            .commit_login(account(), "readded-refresh", "readded-access")
            .unwrap();
        assert_eq!(index.accounts.borrow().entries[0].generation, 2);
        assert!(session.save_refresh("late-rotation").is_err());
        assert!(session.commit(first, "late-access").is_err());
        assert_eq!(
            credentials
                .read(&account(), "minecraft")
                .unwrap()
                .unwrap()
                .as_str(),
            "readded-access"
        );
    }
    #[test]
    fn failed_refresh_commit_keeps_rotation_and_releases_session_for_retry() {
        let (index, credentials) = fixture();
        let service = AccountService::new(&index, &credentials);
        let mut session = service.begin_refresh(&account()).unwrap();
        assert!(session.commit(account(), "access-before-oauth").is_err());
        session.save_refresh("rotated-refresh").unwrap();
        index.fail.set(true);
        assert!(session.commit(account(), "new-access").is_err());
        assert_original(&index, &credentials);
        assert_eq!(index.accounts.borrow().revision, 0);
        drop(session);
        let next = service.begin_refresh(&account()).unwrap();
        assert_eq!(next.token(), "rotated-refresh");
    }
    #[test]
    fn exhausted_revision_fails_before_any_credential_write() {
        let (index, credentials) = fixture();
        index.accounts.borrow_mut().revision = u64::MAX;
        assert!(AccountService::new(&index, &credentials)
            .commit_login(account(), "new", "access")
            .is_err());
        assert_original(&index, &credentials);
        assert_eq!(
            credentials
                .read(&account(), "refresh")
                .unwrap()
                .unwrap()
                .as_str(),
            "old-refresh"
        );
    }
    #[test]
    fn failed_interactive_login_that_preserves_rotation_also_invalidates_old_refresh() {
        for after_oauth_save in [false, true] {
            let (index, credentials) = fixture();
            let service = AccountService::new(&index, &credentials);
            let mut session = service.begin_refresh(&account()).unwrap();
            if after_oauth_save {
                session.save_refresh("old-session-rotation").unwrap();
            }
            index.fail.set(true);
            assert!(service
                .commit_login(account(), "preserved-new-refresh", "failed-login-access")
                .is_err());
            assert_eq!(index.accounts.borrow().revision, 0);
            if after_oauth_save {
                assert!(session.commit(account(), "late-access").is_err());
            } else {
                assert!(session.save_refresh("late-rotation").is_err());
            }
            assert_original(&index, &credentials);
            drop(session);
            let next = service.begin_refresh(&account()).unwrap();
            assert_eq!(next.token(), "preserved-new-refresh");
        }
    }
    #[test]
    fn another_process_cannot_enter_the_account_mutation_until_lease_is_released() {
        use std::{
            thread,
            time::{Duration, Instant},
        };
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("accounts.lock");
        let held = AccountIndexLock::acquire(&path).unwrap();
        let ready = workspace.path.join("blocked");
        let released = workspace.path.join("released");
        let acquired = workspace.path.join("acquired");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "account_service::tests::account_lock_child_probe",
            ])
            .env("TUI_ACCOUNT_LOCK_PROBE", &path)
            .env("TUI_ACCOUNT_LOCK_READY", &ready)
            .env("TUI_ACCOUNT_LOCK_RELEASED", &released)
            .env("TUI_ACCOUNT_LOCK_ACQUIRED", &acquired)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut child = crate::process_service::ManagedProcess(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "account lock probe exited early"
            );
            assert!(
                Instant::now() < deadline,
                "account lock probe did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!acquired.exists());
        drop(held);
        fs::write(&released, b"released").unwrap();
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "account lock probe did not finish"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(child.wait().unwrap().success());
        assert_eq!(fs::read(acquired).unwrap(), b"acquired");
    }
    #[test]
    #[ignore = "subprocess helper invoked by account index lock test"]
    fn account_lock_child_probe() {
        let Some(path) = std::env::var_os("TUI_ACCOUNT_LOCK_PROBE") else {
            return;
        };
        assert!(AccountIndexLock::acquire(Path::new(&path)).is_err());
        fs::write(
            std::env::var_os("TUI_ACCOUNT_LOCK_READY").unwrap(),
            b"blocked",
        )
        .unwrap();
        let released = std::env::var_os("TUI_ACCOUNT_LOCK_RELEASED").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !Path::new(&released).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent did not release account lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _lease = AccountIndexLock::acquire(Path::new(&path)).unwrap();
        fs::write(
            std::env::var_os("TUI_ACCOUNT_LOCK_ACQUIRED").unwrap(),
            b"acquired",
        )
        .unwrap();
    }
}
