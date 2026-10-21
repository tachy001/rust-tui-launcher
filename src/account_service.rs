use crate::accounts::{Account, AccountKind, Accounts};
use anyhow::{bail, Context, Result};
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
pub(crate) trait AccountIndexStore {
    fn load(&self) -> Result<Accounts>;
    /// A failed save must leave the previous index intact.
    fn save(&self, accounts: &Accounts) -> Result<()>;
}
pub(crate) struct FileAccountIndex;
impl AccountIndexStore for FileAccountIndex {
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
        let mut accounts = self.index.load()?;
        accounts.upsert(Account {
            id: format!("offline:{}", crate::backend::offline_uuid(name)),
            name: name.into(),
            kind: AccountKind::Offline,
            client_id: String::new(),
            expires_at: 0,
            xuid: String::new(),
        });
        self.index.save(&accounts)?;
        Ok(accounts)
    }
    pub(crate) fn commit_login(
        &self,
        account: Account,
        refresh: &str,
        minecraft: &str,
        select: bool,
    ) -> Result<()> {
        if account.kind != AccountKind::Microsoft || refresh.is_empty() || minecraft.is_empty() {
            bail!("正版登录结果缺少有效账户或凭证");
        }
        let mut accounts = self.index.load()?;
        if !select && !accounts.entries.iter().any(|old| old.id == account.id) {
            bail!("刷新期间账户已被移除，请重新登录");
        }
        let previous_active = accounts.active.clone();
        let previous_access = self.credentials.read(&account, "minecraft")?;
        // Keep the newest refresh token durable even when a later stage fails.
        self.credentials.write(&account, "refresh", refresh)?;
        let result = (|| -> Result<()> {
            self.credentials.write(&account, "minecraft", minecraft)?;
            accounts.upsert(account.clone());
            if !select {
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
    }
    impl AccountIndexStore for Index {
        fn load(&self) -> Result<Accounts> {
            Ok(self.accounts.borrow().clone())
        }
        fn save(&self, accounts: &Accounts) -> Result<()> {
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
        fn read(&self, _account: &Account, kind: &str) -> Result<Option<Zeroizing<String>>> {
            Ok(self.values.borrow().get(kind).cloned())
        }
        fn write(&self, _account: &Account, kind: &str, value: &str) -> Result<()> {
            if self.fail_write.borrow().as_deref() == Some(kind) {
                self.fail_write.borrow_mut().take();
                bail!("凭证保存失败");
            }
            self.values
                .borrow_mut()
                .insert(kind.into(), Zeroizing::new(value.into()));
            Ok(())
        }
        fn delete(&self, _account: &Account, kind: &str) -> Result<()> {
            if self.fail_delete.borrow().as_deref() == Some(kind) {
                self.fail_delete.borrow_mut().take();
                bail!("凭证删除失败");
            }
            self.values.borrow_mut().remove(kind);
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
                .commit_login(next, "new-refresh", "new-access", true)
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
        AccountService::new(&index, &credentials)
            .commit_login(account(), "new-refresh", "new-access", false)
            .unwrap();
        assert!(index.accounts.borrow().active.is_none());
        index.accounts.borrow_mut().entries.clear();
        assert!(AccountService::new(&index, &credentials)
            .commit_login(account(), "latest", "access", false)
            .is_err());
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
        assert!(AccountService::new(&index, &credentials)
            .commit_login(account(), "new-refresh", "new-access", false)
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
}
