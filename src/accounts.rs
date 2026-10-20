use crate::{backend::offline_uuid, config::Config, storage::atomic_write};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Offline,
    Microsoft,
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
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Accounts {
    pub active: Option<String>,
    pub entries: Vec<Account>,
}
impl Accounts {
    pub fn load() -> Result<Self> {
        Self::read(Path::new(".tui-mc/accounts.json"))
    }
    fn read(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        serde_json::from_slice(&fs::read(path)?).context("账户列表格式错误")
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
        let config = Config {
            username: name.into(),
            ..Config::default()
        };
        config.validate()?;
        let uuid = offline_uuid(name);
        let id = format!("offline:{uuid}");
        self.upsert(Account {
            id,
            name: name.into(),
            kind: AccountKind::Offline,
            client_id: String::new(),
            expires_at: 0,
            xuid: String::new(),
        });
        self.save()
    }
    pub fn upsert(&mut self, account: Account) {
        self.entries.retain(|entry| entry.id != account.id);
        self.active = Some(account.id.clone());
        self.entries.push(account);
    }
    pub fn select(&mut self, id: &str) -> Result<()> {
        if !self.entries.iter().any(|entry| entry.id == id) {
            bail!("账户不存在");
        }
        self.active = Some(id.into());
        self.save()
    }
    pub fn remove(&mut self, id: &str) -> Result<()> {
        let account = self
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .context("账户不存在")?;
        if account.kind == AccountKind::Microsoft {
            for kind in ["refresh", "minecraft"] {
                crate::auth::delete_secret(account, kind)?;
            }
        }
        self.entries.retain(|entry| entry.id != id);
        if self.active.as_deref() == Some(id) {
            self.active = self.entries.first().map(|entry| entry.id.clone());
        }
        self.save()
    }
}
