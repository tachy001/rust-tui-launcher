use crate::task::TaskControl;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub(crate) const AUTHORIZE_URL: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";

pub(crate) fn client_id(override_id: &str) -> Result<String> {
    resolve_client_id(
        override_id,
        option_env!("RUST_TUI_LAUNCHER_MICROSOFT_CLIENT_ID"),
    )
}

fn resolve_client_id(override_id: &str, bundled: Option<&str>) -> Result<String> {
    let value = if override_id.trim().is_empty() {
        bundled.unwrap_or_default().trim()
    } else {
        override_id.trim()
    };
    if value.is_empty() {
        bail!("此构建尚未配置微软登录应用。发布者需注册本启动器的微软应用，并在构建时设置 RUST_TUI_LAUNCHER_MICROSOFT_CLIENT_ID；普通用户无需申请应用 ID。自编译用户可使用高级配置覆盖。");
    }
    crate::auth::validate_client_id(value)?;
    Ok(value.into())
}

pub(crate) trait BrowserOpener {
    fn open(&self, url: &str) -> Result<()>;
}

pub(crate) struct SystemBrowser;
impl BrowserOpener for SystemBrowser {
    fn open(&self, url: &str) -> Result<()> {
        let url = Url::parse(url).context("无效登录地址")?;
        if url.scheme() != "https"
            || !matches!(
                url.host_str(),
                Some(
                    "login.microsoftonline.com"
                        | "login.microsoft.com"
                        | "microsoft.com"
                        | "www.microsoft.com"
                )
            )
            || !url.username().is_empty()
            || url.password().is_some()
        {
            bail!("拒绝打开非微软 HTTPS 登录地址");
        }
        #[cfg(target_os = "windows")]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let mut command = Command::new("rundll32.exe");
            command.args(["url.dll,FileProtocolHandler", url.as_str()]);
            command.creation_flags(0x08000000);
            command
        };
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = Command::new("open");
            command.arg(url.as_str());
            command
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let mut command = {
            let mut command = Command::new("xdg-open");
            command.arg(url.as_str());
            command
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("无法请求系统浏览器，请复制登录链接手动打开")?;
        // Reap the short-lived platform helper without blocking the login task.
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// Own the loopback listener and the PKCE secrets for exactly one login attempt.
pub(crate) struct BrowserLogin {
    listener: TcpListener,
    verifier: Zeroizing<String>,
    state: Zeroizing<String>,
    redirect: String,
    port: u16,
}

enum Callback {
    Ignore,
    Code(Zeroizing<String>),
    Denied,
    Failed,
}

fn random_secret() -> Result<Zeroizing<String>> {
    let mut bytes = Zeroizing::new([0_u8; 32]);
    getrandom::getrandom(bytes.as_mut())
        .map_err(|_| anyhow::anyhow!("无法取得安全随机数，取消微软登录"))?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes.as_slice())))
}

impl BrowserLogin {
    pub(crate) fn bind() -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .context("无法创建本地微软登录回调，请使用设备码登录")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            verifier: random_secret()?,
            state: random_secret()?,
            redirect: format!("http://localhost:{port}/auth-response"),
            port,
        })
    }

    pub(crate) fn authorization_url(&self, client_id: &str, scope: &str) -> Result<Url> {
        let mut url = Url::parse(AUTHORIZE_URL)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        url.query_pairs_mut().extend_pairs([
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", self.redirect.as_str()),
            ("scope", scope),
            ("prompt", "select_account"),
            ("code_challenge", challenge.as_str()),
            ("state", self.state.as_str()),
            ("code_challenge_method", "S256"),
            ("response_mode", "query"),
        ]);
        Ok(url)
    }

    pub(crate) fn verifier(&self) -> &str {
        &self.verifier
    }
    pub(crate) fn redirect_uri(&self) -> &str {
        &self.redirect
    }

    pub(crate) fn wait(
        &self,
        control: &TaskControl,
        timeout: Duration,
    ) -> Result<Zeroizing<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            control.check()?;
            if Instant::now() >= deadline {
                bail!("浏览器登录已超时，请重新登录");
            }
            match self.listener.accept() {
                Ok((mut stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    stream.set_nonblocking(true)?;
                    let request = self.read_request(&mut stream, control, deadline)?;
                    let callback = request
                        .as_deref()
                        .map(|s| Self::callback(s, &self.state, self.port))
                        .unwrap_or(Callback::Ignore);
                    let (status, message) = match &callback {
                        Callback::Code(_) => ("200 OK", "Microsoft authorization received. Return to Rust TUI Launcher to finish Xbox and Minecraft verification. You may close this tab."),
                        Callback::Denied => ("200 OK", "Microsoft authorization was declined. Return to Rust TUI Launcher."),
                        Callback::Failed => ("200 OK", "Microsoft authorization failed. Return to Rust TUI Launcher and check the application configuration."),
                        Callback::Ignore => ("400 Bad Request", "Invalid login callback. Keep the launcher open and finish signing in through its login link."),
                    };
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{message}", message.len());
                    // The code is already received; a closed browser socket must not lose it.
                    let _ = stream.write(response.as_bytes());
                    match callback {
                        Callback::Code(code) => return Ok(code),
                        Callback::Denied => bail!("用户拒绝或取消了微软授权"),
                        Callback::Failed => {
                            bail!("微软网页授权失败，请检查应用的个人账户与回调地址配置后重试")
                        }
                        Callback::Ignore => {}
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50))
                }
                Err(error) => return Err(error).context("读取微软登录回调失败"),
            }
        }
    }

    fn read_request(
        &self,
        stream: &mut TcpStream,
        control: &TaskControl,
        deadline: Instant,
    ) -> Result<Option<Zeroizing<String>>> {
        let read_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        let mut bytes = Zeroizing::new(Vec::new());
        let mut buffer = Zeroizing::new([0_u8; 1024]);
        while bytes.len() < 16 * 1024 && Instant::now() < read_deadline {
            control.check()?;
            match stream.read(buffer.as_mut()) {
                Ok(0) => return Ok(None),
                Ok(count) => {
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                        return Ok(std::str::from_utf8(&bytes)
                            .ok()
                            .map(|text| Zeroizing::new(text.into())));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(_) => return Ok(None),
            }
        }
        Ok(None)
    }

    fn callback(request: &str, state: &str, port: u16) -> Callback {
        let mut lines = request.split("\r\n");
        let Some(first) = lines.next() else {
            return Callback::Ignore;
        };
        let parts: Vec<_> = first.split(' ').collect();
        if parts.len() != 3
            || parts[0] != "GET"
            || !matches!(parts[2], "HTTP/1.1" | "HTTP/1.0")
            || !parts[1].starts_with("/auth-response?")
        {
            return Callback::Ignore;
        }
        let hosts: Vec<_> = lines
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim())
            .collect();
        if hosts.len() != 1
            || ![format!("localhost:{port}"), format!("127.0.0.1:{port}")]
                .iter()
                .any(|host| hosts[0] == host)
        {
            return Callback::Ignore;
        }
        let Ok(url) = Url::parse(&format!("http://localhost{}", parts[1])) else {
            return Callback::Ignore;
        };
        if url.path() != "/auth-response" || url.fragment().is_some() {
            return Callback::Ignore;
        }
        let pairs: Vec<_> = url.query_pairs().collect();
        let single = |key: &str| {
            let mut values = pairs
                .iter()
                .filter(|(name, _)| name == key)
                .map(|(_, value)| value.as_ref());
            let value = values.next()?;
            if values.next().is_some() {
                None
            } else {
                Some(value)
            }
        };
        if single("state") != Some(state) {
            return Callback::Ignore;
        }
        let has_code = pairs.iter().any(|(key, _)| key == "code");
        let has_error = pairs.iter().any(|(key, _)| key == "error");
        if has_error && !has_code && single("error").is_some_and(|error| !error.is_empty()) {
            return if single("error") == Some("access_denied") {
                Callback::Denied
            } else {
                Callback::Failed
            };
        }
        if has_error {
            return Callback::Ignore;
        }
        match single("code") {
            Some(code) if !code.is_empty() && !code.chars().any(char::is_control) => {
                Callback::Code(Zeroizing::new(code.into()))
            }
            _ => Callback::Ignore,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_id_is_used_without_user_configuration() {
        let bundled = "12345678-1234-4321-8123-123456789abc";
        let custom = "87654321-4321-4321-8123-123456789abc";
        assert_eq!(resolve_client_id("", Some(bundled)).unwrap(), bundled);
        assert_eq!(resolve_client_id(custom, Some(bundled)).unwrap(), custom);
        assert!(resolve_client_id("", None).is_err());
        assert!(resolve_client_id("bad", Some(bundled)).is_err());
        assert!(resolve_client_id("", Some("bad")).is_err());
    }

    fn callback(query: &str) -> Callback {
        BrowserLogin::callback(
            &format!("GET /auth-response?{query} HTTP/1.1\r\nHost: localhost:12345\r\n\r\n"),
            "expected",
            12345,
        )
    }

    #[test]
    fn accepts_only_bound_state_and_unambiguous_callback() {
        let Callback::Code(code) = callback("state=expected&code=a%2Bb") else {
            panic!("valid callback rejected")
        };
        assert_eq!(code.as_str(), "a+b");
        assert!(matches!(
            callback("state=expected&error=access_denied"),
            Callback::Denied
        ));
        assert!(matches!(
            callback("state=expected&error=server_error"),
            Callback::Failed
        ));
        for query in [
            "code=secret",
            "state=wrong&code=secret",
            "state=expected&state=expected&code=x",
            "state=expected&code=x&code=y",
            "state=expected&code=",
            "state=expected&code=x&error=access_denied",
            "state=expected&code=x%0A",
            "state=expected&code=x#fragment",
        ] {
            assert!(matches!(callback(query), Callback::Ignore), "{query}");
        }
        for request in [
            "GET /favicon.ico?state=expected&code=x HTTP/1.1\r\nHost: localhost:12345\r\n\r\n",
            "POST /auth-response?state=expected&code=x HTTP/1.1\r\nHost: localhost:12345\r\n\r\n",
            "GET /auth-response?state=expected&code=x HTTP/1.1\r\nHost: attacker.example\r\n\r\n",
            "GET /auth-response?state=expected&code=x HTTP/1.1\r\nHost: localhost:12345\r\nHost: localhost:12345\r\n\r\n",
        ] {
            assert!(matches!(BrowserLogin::callback(request, "expected", 12345), Callback::Ignore));
        }
    }

    #[test]
    fn loopback_receives_browser_callback_and_pkce_parameters() {
        let mut session = BrowserLogin::bind().unwrap();
        let second = BrowserLogin::bind().unwrap();
        assert_ne!(session.state.as_str(), second.state.as_str());
        assert_ne!(session.verifier(), second.verifier());
        assert_eq!(session.verifier().len(), 43);
        // RFC 7636 appendix B reference vector, not a self-comparison only.
        session.verifier = Zeroizing::new("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        let url = session
            .authorization_url("client", "XboxLive.signin offline_access")
            .unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["redirect_uri"], session.redirect_uri());
        assert_eq!(pairs["state"], session.state.as_str());
        assert_eq!(
            pairs["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(
            pairs["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(session.verifier.as_bytes()))
        );
        assert!(!url.as_str().contains(session.verifier()));
        let request = format!("GET /auth-response?state={}&code=local-test-code HTTP/1.1\r\nHost: localhost:{}\r\n\r\n", session.state.as_str(), session.port);
        let address = session.listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket.write_all(request.as_bytes()).unwrap();
            let mut reply = String::new();
            socket.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with("HTTP/1.1 200 OK"));
            assert!(!reply.contains("local-test-code"));
        });
        let code = session
            .wait(&TaskControl::default(), Duration::from_secs(5))
            .unwrap();
        writer.join().unwrap();
        assert_eq!(code.as_str(), "local-test-code");
        let control = TaskControl::default();
        control.cancel();
        assert!(second.wait(&control, Duration::from_secs(5)).is_err());
        assert!(second
            .wait(&TaskControl::default(), Duration::ZERO)
            .is_err());
    }

    #[test]
    fn browser_rejects_external_targets_without_launching_a_process() {
        for url in [
            "http://login.microsoftonline.com/",
            "https://attacker.example/",
            "file:///login",
            "https://login.microsoftonline.com.attacker.example/",
            "https://name@login.microsoftonline.com/",
        ] {
            assert!(SystemBrowser.open(url).is_err());
        }
    }
}
