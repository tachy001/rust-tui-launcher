use crate::account_service::CredentialStore;
use crate::{
    accounts::{Account, AccountKind, Accounts},
    backend::Backend,
    config::Config,
};
use anyhow::{bail, Context, Result};
use reqwest::blocking::{RequestBuilder, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const DEVICE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const SCOPE: &str = "XboxLive.signin offline_access";

pub struct Session {
    pub name: String,
    pub uuid: String,
    pub access_token: Zeroizing<String>,
    pub xuid: String,
    pub client_id: String,
    pub user_type: &'static str,
}
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "poll_default")]
    interval: u64,
}
fn poll_default() -> u64 {
    5
}
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct OAuthToken {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
}
fn read_secret(account: &Account, kind: &str) -> Result<Zeroizing<String>> {
    crate::account_service::SystemCredentials
        .read(account, kind)?
        .context("读取账户凭证失败，请重新登录")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn validate_client_id(value: &str) -> Result<()> {
    let compact = value.replace('-', "");
    if value.len() != 36
        || compact.len() != 32
        || !compact.bytes().all(|b| b.is_ascii_hexdigit())
        || value.as_bytes().get(8) != Some(&b'-')
        || value.as_bytes().get(13) != Some(&b'-')
        || value.as_bytes().get(18) != Some(&b'-')
        || value.as_bytes().get(23) != Some(&b'-')
    {
        bail!("请先在设置中填写启动器自己的微软 OAuth 客户端 ID（GUID），并为应用启用个人账户与公共客户端设备代码流程");
    }
    Ok(())
}
fn checked_response(response: Response, stage: &str) -> Result<Value> {
    let status = response.status();
    if !status.is_success() {
        bail!(
            "{stage}失败（HTTP {}）；请检查应用授权、账户权限或重新登录",
            status.as_u16()
        );
    }
    response
        .json()
        .with_context(|| format!("{stage}响应格式错误"))
}
fn send_json(request: RequestBuilder, stage: &str) -> Result<Value> {
    checked_response(
        request
            .send()
            .with_context(|| format!("{stage}网络请求失败"))?,
        stage,
    )
}
fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("认证响应缺少 {name}"))
}
fn profile(value: &Value) -> Result<(String, String)> {
    let uuid = field(value, "id")?;
    if uuid.len() != 32 || !uuid.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("Minecraft 返回了无效 UUID");
    }
    let name = field(value, "name")?;
    let config = Config {
        username: name.into(),
        ..Config::default()
    };
    config.validate()?;
    Ok((uuid.into(), name.into()))
}
fn device_error(error: &str) -> Result<bool> {
    match error {
        "authorization_pending" => Ok(false),
        "slow_down" => Ok(true),
        "authorization_declined" | "access_denied" => bail!("用户已拒绝授权"),
        "expired_token" => bail!("设备登录码已过期，请重新登录"),
        "bad_verification_code" => bail!("设备登录码无效，请重新登录"),
        _ => bail!("微软认证失败：{error}"),
    }
}
fn xsts_error(code: u64) -> &'static str {
    match code {
        2148916233 => "该微软账户还没有 Xbox 档案，请先在 Xbox 网站创建",
        2148916235 => "当前地区不支持 Xbox Live",
        2148916236 | 2148916237 => "Xbox 账户需要完成年龄验证",
        2148916238 => "儿童账户需要加入微软家庭并获得家长授权",
        _ => "Xbox 授权失败，请检查账号限制与应用权限",
    }
}
impl Backend {
    fn auth_wait(&self, duration: Duration) -> Result<()> {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            self.control.check()?;
            thread::sleep(
                Duration::from_millis(100).min(until.saturating_duration_since(Instant::now())),
            );
        }
        self.control.check()
    }
    pub fn login_microsoft(&self, config: &Config) -> Result<String> {
        validate_client_id(&config.microsoft_client_id)?;
        self.control.check()?;
        let device: DeviceCode = serde_json::from_value(send_json(
            self.client.post(DEVICE_URL).form(&[
                ("client_id", config.microsoft_client_id.as_str()),
                ("scope", SCOPE),
            ]),
            "获取设备登录码",
        )?)?;
        let uri = reqwest::Url::parse(&device.verification_uri)?;
        if uri.scheme() != "https" {
            bail!("登录地址不是 HTTPS");
        }
        self.log(format!(
            "请在浏览器打开 {}，输入代码 {}（{} 分钟内有效）",
            device.verification_uri,
            device.user_code,
            device.expires_in / 60
        ));
        let deadline = Instant::now() + Duration::from_secs(device.expires_in);
        let mut interval = device.interval.max(1);
        let token = loop {
            self.auth_wait(Duration::from_secs(interval))?;
            if Instant::now() >= deadline {
                bail!("设备登录码已过期");
            }
            let response = self
                .client
                .post(TOKEN_URL)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", config.microsoft_client_id.as_str()),
                    ("device_code", device.device_code.as_str()),
                ])
                .send()
                .context("轮询微软登录失败")?;
            let status = response.status();
            let value: Value = response.json().context("微软登录响应格式错误")?;
            if status.is_success() {
                break serde_json::from_value::<OAuthToken>(value)?;
            }
            let error = value["error"].as_str().unwrap_or("unknown_error");
            if device_error(error)? {
                interval = interval.saturating_add(5);
            }
        };
        self.finish_auth(&config.microsoft_client_id, &token, None)
    }
    fn finish_auth(
        &self,
        client_id: &str,
        oauth: &OAuthToken,
        refresh_session: Option<&crate::account_service::RefreshSession<'_>>,
    ) -> Result<String> {
        self.control.check()?;
        self.log("微软授权完成，正在验证 Xbox 与 Minecraft 资格…");
        let xbox=send_json(self.client.post("https://user.auth.xboxlive.com/user/authenticate").json(&json!({"Properties":{"AuthMethod":"RPS","SiteName":"user.auth.xboxlive.com","RpsTicket":format!("d={}",oauth.access_token)},"RelyingParty":"http://auth.xboxlive.com","TokenType":"JWT"})),"Xbox 登录")?;
        let uhs = xbox
            .pointer("/DisplayClaims/xui/0/uhs")
            .and_then(Value::as_str)
            .context("Xbox 没有返回用户哈希")?;
        self.control.check()?;
        let response=self.client.post("https://xsts.auth.xboxlive.com/xsts/authorize").json(&json!({"Properties":{"SandboxId":"RETAIL","UserTokens":[field(&xbox,"Token")?]},"RelyingParty":"rp://api.minecraftservices.com/","TokenType":"JWT"})).send().context("Xbox XSTS 网络请求失败")?;
        let status = response.status();
        let xsts: Value = response.json().context("XSTS 响应格式错误")?;
        if !status.is_success() {
            bail!("{}", xsts_error(xsts["XErr"].as_u64().unwrap_or(0)));
        }
        if xsts
            .pointer("/DisplayClaims/xui/0/uhs")
            .and_then(Value::as_str)
            != Some(uhs)
        {
            bail!("Xbox 用户哈希不一致，拒绝继续认证");
        }
        self.control.check()?;
        let minecraft = send_json(
            self.client
                .post("https://api.minecraftservices.com/authentication/login_with_xbox")
                .json(&json!({"identityToken":format!("XBL3.0 x={uhs};{}",field(&xsts,"Token")?)})),
            "Minecraft 登录",
        )?;
        let access = field(&minecraft, "access_token")?;
        let entitlements = send_json(
            self.client
                .get("https://api.minecraftservices.com/entitlements/mcstore")
                .bearer_auth(access),
            "Minecraft 授权检查",
        )?;
        if entitlements["items"]
            .as_array()
            .is_none_or(|items| items.is_empty())
        {
            bail!("该账户没有有效的 Minecraft Java 版授权");
        }
        let user = send_json(
            self.client
                .get("https://api.minecraftservices.com/minecraft/profile")
                .bearer_auth(access),
            "获取 Minecraft 档案",
        )?;
        let (uuid, name) = profile(&user)?;
        let account = Account {
            id: format!("microsoft:{uuid}"),
            name: name.clone(),
            kind: AccountKind::Microsoft,
            client_id: client_id.into(),
            expires_at: now().saturating_add(minecraft["expires_in"].as_u64().unwrap_or(3600)),
            xuid: xsts
                .pointer("/DisplayClaims/xui/0/xid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            generation: 0,
        };
        if oauth.refresh_token.is_empty() {
            bail!("微软未返回刷新令牌，请确认请求了 offline_access 权限");
        }
        if refresh_session.is_some_and(|session| session.account().id != account.id) {
            bail!("刷新后的 Minecraft 账户身份不一致，请重新登录");
        }
        self.control.check()?;
        if let Some(session) = refresh_session {
            session.commit(account, access)?;
        } else {
            crate::account_service::AccountService::new(
                &crate::account_service::FileAccountIndex,
                &crate::account_service::SystemCredentials,
            )
            .commit_login(account, &oauth.refresh_token, access)?;
        }
        Ok(format!("正版账户已登录：{name}"))
    }
    fn refresh_account(&self, account: &Account) -> Result<()> {
        validate_client_id(&account.client_id)?;
        self.control.check()?;
        let mut refresh_session = crate::account_service::AccountService::new(
            &crate::account_service::FileAccountIndex,
            &crate::account_service::SystemCredentials,
        )
        .begin_refresh(account)?;
        let value = send_json(
            self.client.post(TOKEN_URL).form(&[
                ("grant_type", "refresh_token"),
                ("client_id", account.client_id.as_str()),
                ("refresh_token", refresh_session.token()),
                ("scope", SCOPE),
            ]),
            "刷新微软令牌",
        )?;
        let mut oauth: OAuthToken = serde_json::from_value(value)?;
        if oauth.refresh_token.is_empty() {
            oauth.refresh_token = refresh_session.token().to_owned();
        }
        refresh_session.save_refresh(&oauth.refresh_token)?;
        self.finish_auth(&account.client_id, &oauth, Some(&refresh_session))?;
        Ok(())
    }
    pub fn account_session(&self, config: &Config) -> Result<Session> {
        let mut accounts = Accounts::load()?;
        let Some(account) = accounts.active().cloned() else {
            return Ok(offline(&config.username));
        };
        if account.kind == AccountKind::Offline {
            return Ok(offline(&account.name));
        }
        if account.expires_at <= now().saturating_add(120) {
            self.refresh_account(&account)?;
            accounts = Accounts::load()?;
        }
        let account = accounts
            .entries
            .iter()
            .find(|a| a.id == account.id)
            .context("刷新后账户信息丢失")?;
        Ok(Session {
            name: account.name.clone(),
            uuid: account
                .id
                .strip_prefix("microsoft:")
                .context("账户 ID 无效")?
                .into(),
            access_token: read_secret(account, "minecraft")?,
            xuid: account.xuid.clone(),
            client_id: account.client_id.clone(),
            user_type: "msa",
        })
    }
}
fn offline(name: &str) -> Session {
    Session {
        name: name.into(),
        uuid: crate::backend::offline_uuid(name),
        access_token: Zeroizing::new("0".into()),
        xuid: String::new(),
        client_id: String::new(),
        user_type: "legacy",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oauth_codes_have_correct_flow() {
        assert!(!device_error("authorization_pending").unwrap());
        assert!(device_error("slow_down").unwrap());
        assert!(device_error("expired_token").is_err());
        assert!(device_error("authorization_declined").is_err());
    }
    #[test]
    fn validates_own_client_id() {
        assert!(validate_client_id("").is_err());
        assert!(validate_client_id("00000000-0000-4000-8000-000000000000").is_ok());
    }
    #[test]
    fn rejects_invalid_minecraft_profile() {
        assert!(profile(&json!({"id":"../../bad","name":"Player"})).is_err());
        assert!(profile(&json!({"id":"0123456789abcdef0123456789abcdef","name":"Player"})).is_ok());
    }
    #[test]
    fn account_index_contains_no_credentials() {
        let mut accounts = Accounts::default();
        accounts.upsert(Account {
            id: "microsoft:uuid".into(),
            name: "Player".into(),
            kind: AccountKind::Microsoft,
            client_id: "app".into(),
            expires_at: 123,
            xuid: "123".into(),
            generation: 0,
        });
        let json = serde_json::to_string(&accounts).unwrap();
        assert!(!json.contains("token"));
        assert!(!json.contains("secret"));
    }
}
