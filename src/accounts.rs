use crate::{
    account_service::{AccountService, FileAccountIndex, SystemCredentials},
    storage::atomic_write,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Offline,
    Microsoft,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Workspace;

    #[test]
    fn missing_index_is_empty_but_unreadable_or_invalid_index_is_an_error() {
        let workspace = Workspace::new();
        let path = workspace.path.join("accounts.json");
        assert!(Accounts::read(&path).unwrap().entries.is_empty());
        assert!(Accounts::read(&workspace.path).is_err());
        fs::write(&path, b"{broken").unwrap();
        assert!(Accounts::read(&path).is_err());
    }

    #[test]
    fn rejects_ambiguous_identity_and_missing_active_account() {
        let workspace = Workspace::new();
        for (number, json) in [
            r#"{"entries":[{"id":"","name":"Player","kind":"offline"}]}"#,
            r#"{"entries":[{"id":"same","name":"One","kind":"offline"},{"id":"same","name":"Two","kind":"offline"}]}"#,
            r#"{"active":"missing","entries":[]}"#,
        ]
        .iter()
        .enumerate()
        {
            let path = workspace.path.join(format!("invalid-{number}.json"));
            fs::write(&path, json).unwrap();
            assert!(Accounts::read(&path).is_err());
        }
    }

    #[test]
    fn reads_existing_index_with_default_optional_fields() {
        let workspace = Workspace::new();
        let path = workspace.path.join("accounts.json");
        fs::write(
            &path,
            r#"{"active":"offline:1","entries":[{"id":"offline:1","name":"Player","kind":"offline"}]}"#,
        )
        .unwrap();
        let accounts = Accounts::read(&path).unwrap();
        let active = accounts.active().unwrap();
        assert_eq!(active.name, "Player");
        assert!(active.client_id.is_empty());
        assert_eq!(active.expires_at, 0);
        assert_eq!(active.generation, 0);
        assert_eq!(accounts.revision, 0);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub kind: AccountKind,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub xuid: String,
    #[serde(default)]
    pub generation: u64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Accounts {
    pub revision: u64,
    pub active: Option<String>,
    pub entries: Vec<Account>,
}
impl Accounts {
    pub fn load() -> Result<Self> {
        Self::read(Path::new(".tui-mc/accounts.json"))
    }
    fn read(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error).context("读取账户列表失败"),
        };
        let accounts: Self = serde_json::from_slice(&bytes).context("账户列表格式错误")?;
        let mut seen = std::collections::HashSet::new();
        for account in &accounts.entries {
            if account.id.is_empty() || !seen.insert(&account.id) {
                bail!("账户列表包含空 ID 或重复账户");
            }
        }
        if accounts
            .active
            .as_ref()
            .is_some_and(|id| !seen.contains(id))
        {
            bail!("当前选择的账户不在账户列表中");
        }
        Ok(accounts)
    }
    pub fn save(&self) -> Result<()> {
        atomic_write(
            Path::new(".tui-mc/accounts.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
    pub fn active(&self) -> Option<&Account> {
        self.active
            .as_ref()
            .and_then(|id| self.entries.iter().find(|entry| &entry.id == id))
    }
    pub fn add_offline(&mut self, name: &str) -> Result<()> {
        *self = AccountService::new(&FileAccountIndex, &SystemCredentials).add_offline(name)?;
        Ok(())
    }
    pub fn upsert(&mut self, account: Account) {
        self.entries.retain(|entry| entry.id != account.id);
        self.active = Some(account.id.clone());
        self.entries.push(account);
    }
    pub fn select(&mut self, id: &str) -> Result<()> {
        *self = AccountService::new(&FileAccountIndex, &SystemCredentials).select(id)?;
        Ok(())
    }
    pub fn remove(&mut self, id: &str) -> Result<()> {
        *self = AccountService::new(&FileAccountIndex, &SystemCredentials).remove(id)?;
        Ok(())
    }
}
