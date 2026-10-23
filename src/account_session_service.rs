use crate::{
    accounts::{Account, AccountKind, Accounts},
    auth::Session,
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use zeroize::Zeroizing;

pub(crate) trait SessionAccountSource {
    fn load(&self) -> Result<Accounts>;
    fn minecraft_token(&self, account: &Account) -> Result<Option<Zeroizing<String>>>;
    fn refresh(&self, account: &Account) -> Result<()>;
    fn now(&self) -> u64;
}
pub(crate) struct AccountSessionService<'a> {
    source: &'a dyn SessionAccountSource,
    control: &'a TaskControl,
}
impl<'a> AccountSessionService<'a> {
    pub(crate) fn new(source: &'a dyn SessionAccountSource, control: &'a TaskControl) -> Self {
        Self { source, control }
    }
    pub(crate) fn resolve(&self, username: &str) -> Result<Session> {
        self.control.check()?;
        let accounts = self.source.load()?;
        self.control.check()?;
        let Some(account) = accounts.active() else {
            return Self::offline(username);
        };
        if account.kind == AccountKind::Offline {
            return Self::offline(&account.name);
        }
        Self::validate(account)?;
        if account.expires_at > self.source.now().saturating_add(120) {
            let token = self.source.minecraft_token(account)?;
            self.control.check()?;
            if let Some(token) = token.filter(|token| !token.is_empty()) {
                if account.expires_at > self.source.now() {
                    return Self::session(account, token);
                }
            }
        }
        // Missing/empty cache is recoverable once with the refresh credential.
        // Network, keyring and authentication errors never become offline login.
        self.control.check()?;
        self.source.refresh(account)?;
        self.control.check()?;
        let refreshed = self.source.load()?;
        self.control.check()?;
        let current = refreshed
            .entries
            .iter()
            .find(|entry| entry.id == account.id)
            .context("刷新后账户信息丢失，请重新登录")?;
        if current.kind != AccountKind::Microsoft || current.client_id != account.client_id {
            bail!("刷新后账户类型或应用身份已改变，请重新选择账户并登录");
        }
        Self::validate(current)?;
        let token = self
            .source
            .minecraft_token(current)?
            .filter(|token| !token.is_empty())
            .context("刷新后游戏凭证仍缺失，请重新登录")?;
        self.control.check()?;
        if current.expires_at <= self.source.now() {
            bail!("刷新后的游戏令牌已过期，请重新登录");
        }
        Self::session(current, token)
    }
    fn validate(account: &Account) -> Result<()> {
        crate::auth::validate_client_id(&account.client_id)?;
        let uuid = account
            .id
            .strip_prefix("microsoft:")
            .context("微软账户 ID 无效")?;
        crate::auth::profile(&serde_json::json!({"id":uuid,"name":account.name}))?;
        Ok(())
    }
    fn offline(name: &str) -> Result<Session> {
        crate::config::Config {
            username: name.into(),
            ..Default::default()
        }
        .validate()?;
        Ok(crate::auth::offline(name))
    }
    fn session(account: &Account, access_token: Zeroizing<String>) -> Result<Session> {
        Ok(Session {
            name: account.name.clone(),
            uuid: account
                .id
                .strip_prefix("microsoft:")
                .context("微软账户 ID 无效")?
                .into(),
            access_token,
            xuid: account.xuid.clone(),
            client_id: account.client_id.clone(),
            user_type: "msa",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    fn account() -> Account {
        Account {
            id: "microsoft:0123456789abcdef0123456789abcdef".into(),
            name: "Player".into(),
            kind: AccountKind::Microsoft,
            client_id: "12345678-1234-4321-8123-123456789abc".into(),
            expires_at: 1000,
            xuid: "123".into(),
            generation: 0,
        }
    }
    struct Source {
        initial: Accounts,
        after: Accounts,
        tokens: [Option<String>; 2],
        refreshed: Cell<bool>,
        calls: RefCell<Vec<&'static str>>,
        fail: &'static str,
    }
    fn source() -> Source {
        let mut accounts = Accounts::default();
        accounts.upsert(account());
        Source {
            initial: accounts.clone(),
            after: accounts,
            tokens: [Some("cached-token".into()), Some("refreshed-token".into())],
            refreshed: Cell::new(false),
            calls: RefCell::new(vec![]),
            fail: "",
        }
    }
    impl SessionAccountSource for Source {
        fn load(&self) -> Result<Accounts> {
            self.calls.borrow_mut().push("load");
            Ok(if self.refreshed.get() {
                self.after.clone()
            } else {
                self.initial.clone()
            })
        }
        fn minecraft_token(&self, _: &Account) -> Result<Option<Zeroizing<String>>> {
            self.calls.borrow_mut().push("token");
            if self.fail == "token" {
                bail!("keyring unavailable");
            }
            Ok(self.tokens[usize::from(self.refreshed.get())]
                .clone()
                .map(Zeroizing::new))
        }
        fn refresh(&self, _: &Account) -> Result<()> {
            self.calls.borrow_mut().push("refresh");
            if self.fail == "refresh" {
                bail!("network unavailable");
            }
            assert!(!self.refreshed.replace(true), "refresh loop");
            Ok(())
        }
        fn now(&self) -> u64 {
            100
        }
    }
    #[test]
    fn valid_cached_session_needs_no_refresh() {
        let source = source();
        let control = TaskControl::default();
        let session = AccountSessionService::new(&source, &control)
            .resolve("Fallback")
            .unwrap();
        assert_eq!(session.access_token.as_str(), "cached-token");
        assert_eq!(session.user_type, "msa");
        assert_eq!(*source.calls.borrow(), ["load", "token"]);
    }
    #[test]
    fn missing_empty_or_expiring_cache_is_recovered_once() {
        for (token, expiry) in [
            (None, 1000),
            (Some(String::new()), 1000),
            (Some("old".into()), 220),
        ] {
            let mut source = source();
            source.tokens[0] = token;
            source.initial.entries[0].expires_at = expiry;
            source.after.entries[0].name = "NewName".into();
            let session = AccountSessionService::new(&source, &TaskControl::default())
                .resolve("Fallback")
                .unwrap();
            assert_eq!(session.access_token.as_str(), "refreshed-token");
            assert_eq!(session.name, "NewName");
            assert_eq!(
                source
                    .calls
                    .borrow()
                    .iter()
                    .filter(|call| **call == "refresh")
                    .count(),
                1
            );
        }
    }
    #[test]
    fn changed_missing_expired_or_empty_refresh_results_are_not_launchable() {
        for mode in ["missing", "kind", "client", "uuid", "expired", "token"] {
            let mut source = source();
            source.initial.entries[0].expires_at = 100;
            match mode {
                "missing" => source.after.entries.clear(),
                "kind" => source.after.entries[0].kind = AccountKind::Offline,
                "client" => {
                    source.after.entries[0].client_id =
                        "87654321-4321-4321-8123-123456789abc".into()
                }
                "uuid" => source.after.entries[0].id = "microsoft:invalid".into(),
                "expired" => source.after.entries[0].expires_at = 100,
                _ => source.tokens[1] = Some(String::new()),
            }
            assert!(
                AccountSessionService::new(&source, &TaskControl::default())
                    .resolve("Fallback")
                    .is_err(),
                "{mode}"
            );
        }
    }
    #[test]
    fn credential_and_network_errors_never_downgrade_or_retry_forever() {
        for failure in ["token", "refresh"] {
            let mut source = source();
            source.fail = failure;
            if failure == "refresh" {
                source.initial.entries[0].expires_at = 100;
            }
            let error = AccountSessionService::new(&source, &TaskControl::default())
                .resolve("Fallback")
                .err()
                .unwrap();
            assert!(error.to_string().contains("unavailable"));
            assert!(!source.refreshed.get());
        }
        let source = source();
        let control = TaskControl::default();
        control.cancel();
        assert!(AccountSessionService::new(&source, &control)
            .resolve("Fallback")
            .is_err());
        assert!(source.calls.borrow().is_empty());
    }
    #[test]
    fn offline_selection_does_not_read_credentials_and_invalid_identity_is_rejected() {
        let mut source = source();
        source.initial.entries[0].kind = AccountKind::Offline;
        let session = AccountSessionService::new(&source, &TaskControl::default())
            .resolve("Fallback")
            .unwrap();
        assert_eq!(session.user_type, "legacy");
        assert_eq!(*source.calls.borrow(), ["load"]);
        source.initial.entries[0].kind = AccountKind::Microsoft;
        source.initial.entries[0].name = "bad/name".into();
        source.calls.borrow_mut().clear();
        assert!(AccountSessionService::new(&source, &TaskControl::default())
            .resolve("Fallback")
            .is_err());
        assert_eq!(*source.calls.borrow(), ["load"]);
    }
}
