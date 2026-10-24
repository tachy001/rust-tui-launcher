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
struct DevicePolling {
    deadline: Instant,
    interval: Duration,
}
impl DevicePolling {
    fn new(now: Instant, expires_in: u64, interval: u64) -> Result<Self> {
        if expires_in == 0 {
            bail!("设备登录码已过期");
        }
        Ok(Self {
            deadline: now
                .checked_add(Duration::from_secs(expires_in))
                .context("设备登录码有效期超出支持范围")?,
            interval: Duration::from_secs(interval.max(1)),
        })
    }
    fn wait_duration(&self, now: Instant) -> Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            bail!("设备登录码已过期");
        }
        Ok(self.interval.min(remaining))
    }
    fn slow_down(&mut self) {
        self.interval = self.interval.saturating_add(Duration::from_secs(5));
    }
    fn connection_timeout(&mut self) {
        // RFC 8628 §3.5: reduce polling frequency after a timeout.
        self.interval = self.interval.saturating_mul(2);
    }
}
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct OAuthToken {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn minecraft_expiration(response: &Value, requested_at: u64) -> Result<u64> {
    let lifetime = response["expires_in"]
        .as_u64()
        .filter(|value| *value > 0)
        .context("Minecraft 登录响应缺少有效的令牌有效期")?;
    requested_at
        .checked_add(lifetime)
        .context("Minecraft 令牌有效期超出支持范围")
}
pub(crate) fn validate_client_id(value: &str) -> Result<()> {
    let compact = value.replace('-', "");
    if value.len() != 36
        || compact.len() != 32
        || !compact.bytes().all(|b| b.is_ascii_hexdigit())
        || value.as_bytes().get(8) != Some(&b'-')
        || value.as_bytes().get(13) != Some(&b'-')
        || value.as_bytes().get(18) != Some(&b'-')
        || value.as_bytes().get(23) != Some(&b'-')
    {
        bail!("微软应用 ID 格式无效：发布者内置或高级覆盖的客户端 ID 必须为 GUID");
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
pub(crate) fn profile(value: &Value) -> Result<(String, String)> {
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
struct HttpMinecraftProfile<'a> {
    backend: &'a Backend,
    access: &'a str,
}
impl crate::minecraft_profile_service::MinecraftProfileProvider for HttpMinecraftProfile<'_> {
    fn activate_entitlements(&self) -> Result<()> {
        send_json(
            self.backend
                .client
                .get("https://api.minecraftservices.com/entitlements/mcstore")
                .bearer_auth(self.access),
            "Minecraft 授权检查",
        )?;
        Ok(())
    }
    fn profile(&self) -> Result<Option<Value>> {
        let response = self
            .backend
            .client
            .get("https://api.minecraftservices.com/minecraft/profile")
            .bearer_auth(self.access)
            .send()
            .context("获取 Minecraft 档案网络请求失败")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        checked_response(response, "获取 Minecraft 档案").map(Some)
    }
    fn license(&self) -> Result<Value> {
        send_json(
            self.backend
                .client
                .get("https://api.minecraftservices.com/entitlements/license")
                .bearer_auth(self.access),
            "Minecraft Java 许可证检查",
        )
    }
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
        let until = Instant::now()
            .checked_add(duration)
            .context("认证等待时间超出支持范围")?;
        while Instant::now() < until {
            self.control.check()?;
            thread::sleep(
                Duration::from_millis(100).min(until.saturating_duration_since(Instant::now())),
            );
        }
        self.control.check()
    }
    pub fn login_microsoft(&self, config: &Config) -> Result<String> {
        use crate::browser_login_service::{BrowserLogin, BrowserOpener, SystemBrowser};
        let client_id = crate::browser_login_service::client_id(&config.microsoft_client_id)?;
        self.control.check()?;
        let callback = BrowserLogin::bind()?;
        let url = callback.authorization_url(&client_id, SCOPE)?;
        self.log(format!(
            "请在浏览器完成微软登录；若浏览器未打开，可复制此链接：{url}"
        ));
        self.control.check()?;
        match SystemBrowser.open(url.as_str()) {
            Ok(()) => self.log("已请求打开系统浏览器，等待微软网页授权…"),
            Err(error) => self.log(format!(
                "{error:#}；请手动打开上面的链接，启动器将自动接收授权"
            )),
        }
        let code = callback.wait(&self.control, Duration::from_secs(600))?;
        self.control.check()?;
        let token: OAuthToken = serde_json::from_value(send_json(
            self.client.post(TOKEN_URL).form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id.as_str()),
                ("code", code.as_str()),
                ("code_verifier", callback.verifier()),
                ("redirect_uri", callback.redirect_uri()),
                ("scope", SCOPE),
            ]),
            "交换微软浏览器授权",
        )?)?;
        self.finish_auth(&client_id, &token, None)
    }
    pub fn login_microsoft_device(&self, config: &Config) -> Result<String> {
        use crate::browser_login_service::{BrowserOpener, SystemBrowser};
        let client_id = crate::browser_login_service::client_id(&config.microsoft_client_id)?;
        self.control.check()?;
        let device: DeviceCode = serde_json::from_value(send_json(
            self.client
                .post(DEVICE_URL)
                .form(&[("client_id", client_id.as_str()), ("scope", SCOPE)]),
            "获取设备登录码",
        )?)?;
        let uri = reqwest::Url::parse(&device.verification_uri)?;
        let mut polling = DevicePolling::new(Instant::now(), device.expires_in, device.interval)?;
        if uri.scheme() != "https" {
            bail!("登录地址不是 HTTPS");
        }
        self.log(format!(
            "请在浏览器打开 {}，输入代码 {}（{} 分钟内有效）",
            device.verification_uri,
            device.user_code,
            device.expires_in / 60
        ));
        if let Err(error) = SystemBrowser.open(&device.verification_uri) {
            self.log(format!("{error:#}；请手动打开上面的微软登录地址"));
        }
        let token = loop {
            self.auth_wait(polling.wait_duration(Instant::now())?)?;
            polling.wait_duration(Instant::now())?;
            let response = self
                .client
                .post(TOKEN_URL)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", client_id.as_str()),
                    ("device_code", device.device_code.as_str()),
                ])
                .send();
            self.control.check()?;
            let response = match response {
                Ok(response) => response,
                Err(error) if error.is_timeout() => {
                    polling.connection_timeout();
                    self.log("微软登录轮询超时，将延长间隔后重试；仍可取消登录");
                    continue;
                }
                Err(error) => return Err(error).context("轮询微软登录失败"),
            };
            let status = response.status();
            let value: Value = response.json().context("微软登录响应格式错误")?;
            if status.is_success() {
                break serde_json::from_value::<OAuthToken>(value)?;
            }
            let error = value["error"].as_str().unwrap_or("unknown_error");
            if device_error(error)? {
                polling.slow_down();
            }
        };
        self.finish_auth(&client_id, &token, None)
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
        let requested_at = now();
        let minecraft = send_json(
            self.client
                .post("https://api.minecraftservices.com/authentication/login_with_xbox")
                .json(&json!({"identityToken":format!("XBL3.0 x={uhs};{}",field(&xsts,"Token")?)})),
            "Minecraft 登录",
        )?;
        let access = field(&minecraft, "access_token")?;
        let expires_at = minecraft_expiration(&minecraft, requested_at)?;
        let user = crate::minecraft_profile_service::MinecraftProfileService.fetch(
            &HttpMinecraftProfile {
                backend: self,
                access,
            },
            &self.control,
        )?;
        let (uuid, name) = profile(&user)?;
        let account = Account {
            id: format!("microsoft:{uuid}"),
            name: name.clone(),
            kind: AccountKind::Microsoft,
            client_id: client_id.into(),
            expires_at,
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
        if expires_at <= now() {
            bail!("Minecraft 令牌在验证账户期间已过期，请重新登录");
        }
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
        crate::account_session_service::AccountSessionService::new(self, &self.control)
            .resolve(&config.username)
    }
}
impl crate::account_session_service::SessionAccountSource for Backend {
    fn load(&self) -> Result<Accounts> {
        Accounts::load()
    }
    fn minecraft_token(&self, account: &Account) -> Result<Option<Zeroizing<String>>> {
        crate::account_service::SystemCredentials.read(account, "minecraft")
    }
    fn refresh(&self, account: &Account) -> Result<()> {
        self.refresh_account(account)
    }
    fn now(&self) -> u64 {
        now()
    }
}
pub(crate) fn offline(name: &str) -> Session {
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
    #[test]
    fn device_timeouts_back_off_without_extending_expiration() {
        let now = std::time::Instant::now();
        let mut polling = super::DevicePolling::new(now, 60, 5).unwrap();
        polling.connection_timeout();
        assert_eq!(polling.wait_duration(now).unwrap().as_secs(), 10);
        polling.connection_timeout();
        assert_eq!(polling.wait_duration(now).unwrap().as_secs(), 20);
        polling.slow_down();
        assert_eq!(polling.wait_duration(now).unwrap().as_secs(), 25);
        for _ in 0..100 {
            polling.connection_timeout();
        }
        assert_eq!(polling.wait_duration(now).unwrap().as_secs(), 60);
        assert_eq!(
            polling
                .wait_duration(now + std::time::Duration::from_secs(59))
                .unwrap()
                .as_secs(),
            1
        );
        assert!(polling
            .wait_duration(now + std::time::Duration::from_secs(60))
            .is_err());
    }
    #[test]
    fn minecraft_expiry_is_anchored_to_request_and_rejects_invalid_lifetimes() {
        assert_eq!(
            super::minecraft_expiration(&serde_json::json!({"expires_in": 60}), 100).unwrap(),
            160
        );
        for response in [
            serde_json::json!({}),
            serde_json::json!({"expires_in": 0}),
            serde_json::json!({"expires_in": -1}),
            serde_json::json!({"expires_in": "3600"}),
        ] {
            assert!(super::minecraft_expiration(&response, 100).is_err());
        }
        assert!(
            super::minecraft_expiration(&serde_json::json!({"expires_in": u64::MAX}), 100).is_err()
        );
    }
    #[test]
    fn device_polling_bounds_waits_by_expiration_and_preserves_backoff() {
        let now = std::time::Instant::now();
        let mut polling = super::DevicePolling::new(now, 30, 5).unwrap();
        assert_eq!(polling.wait_duration(now).unwrap().as_secs(), 5);
        polling.slow_down();
        assert_eq!(polling.wait_duration(now).unwrap().as_secs(), 10);
        polling.slow_down();
        assert_eq!(polling.wait_duration(now).unwrap().as_secs(), 15);
        assert_eq!(
            polling
                .wait_duration(now + std::time::Duration::from_secs(29))
                .unwrap()
                .as_secs(),
            1
        );
        assert!(polling
            .wait_duration(now + std::time::Duration::from_secs(30))
            .is_err());
        assert!(super::DevicePolling::new(now, 0, 5).is_err());
        let huge = super::DevicePolling::new(now, 10, u64::MAX).unwrap();
        assert_eq!(huge.wait_duration(now).unwrap().as_secs(), 10);
    }
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
