use crate::{config::Config, metadata::arguments_with_features};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;

pub struct ServerAddress {
    pub host: String,
    pub port: u16,
}
pub fn quickplay_address(input: &str) -> Result<String> {
    Ok(match server_address(input)? {
        Some(address) if address.host.contains(':') => {
            format!("[{}]:{}", address.host, address.port)
        }
        _ => input.trim().to_owned(),
    })
}
pub fn server_address(input: &str) -> Result<Option<ServerAddress>> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    if input
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#' | '@'))
    {
        bail!("服务器地址应为主机名或 IP，可附加端口，不应包含协议、路径或空格");
    }
    let (host, port) = if let Some(rest) = input.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']').context("IPv6 地址缺少右方括号")?;
        host.parse::<std::net::Ipv6Addr>()
            .context("无效 IPv6 地址")?;
        let port = if suffix.is_empty() {
            25565
        } else {
            suffix
                .strip_prefix(':')
                .context("IPv6 端口格式应为 [地址]:端口")?
                .parse::<u16>()
                .context("服务器端口应为 1–65535")?
        };
        (host.to_owned(), port)
    } else if input.parse::<std::net::Ipv6Addr>().is_ok() {
        (input.to_owned(), 25565)
    } else {
        let (host, port) = match input.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().context("服务器端口应为 1–65535")?),
            None => (input, 25565),
        };
        if host.is_empty()
            || host.len() > 253
            || !host
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        {
            bail!("服务器主机名无效，IPv6 指定端口请使用 [地址]:端口");
        }
        (host.to_owned(), port)
    };
    if port == 0 {
        bail!("服务器端口应为 1–65535");
    }
    Ok(Some(ServerAddress { host, port }))
}

pub fn game_arguments(config: &Config, meta: &Value) -> Result<Vec<String>> {
    let server = server_address(&config.server)?;
    let modern = meta["arguments"]["game"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["rules"].as_array().is_some_and(|rules| {
                rules.iter().any(|rule| {
                    rule["features"].get("is_quick_play_multiplayer").is_some()
                        || rule["features"].get("has_quick_plays_support").is_some()
                })
            })
        })
    });
    let features = HashMap::from([
        ("has_custom_resolution", true),
        ("has_quick_plays_support", modern && server.is_some()),
        ("is_quick_play_multiplayer", modern && server.is_some()),
    ]);
    let mut args = if let Some(legacy) = meta["minecraftArguments"].as_str() {
        shlex::split(legacy).context("无法解析旧版启动参数")?
    } else {
        arguments_with_features(&meta["arguments"]["game"], &features)
    };
    if !args.iter().any(|arg| arg == "--width") {
        args.extend(["--width".into(), config.width.to_string()]);
    }
    if !args.iter().any(|arg| arg == "--height") {
        args.extend(["--height".into(), config.height.to_string()]);
    }
    if config.fullscreen && !args.iter().any(|arg| arg == "--fullscreen") {
        args.push("--fullscreen".into());
    }
    if let Some(server) = server {
        if modern {
            if !args.iter().any(|arg| arg == "--quickPlayMultiplayer") {
                args.extend([
                    "--quickPlayMultiplayer".into(),
                    quickplay_address(&config.server)?,
                ]);
            }
        } else {
            args.extend([
                "--server".into(),
                server.host,
                "--port".into(),
                server.port.to_string(),
            ]);
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_domains_and_ipv6_but_rejects_paths_and_bad_ports() {
        assert_eq!(
            server_address("example.org:25566").unwrap().unwrap().port,
            25566
        );
        assert_eq!(server_address("[::1]:25567").unwrap().unwrap().host, "::1");
        assert_eq!(server_address("::1").unwrap().unwrap().port, 25565);
        assert!(server_address("").unwrap().is_none());
        for invalid in [
            "https://example.org",
            "example.org:0",
            "a:65536",
            "a:abc",
            "a b",
            "a/path",
            "[::1]bad",
        ] {
            assert!(server_address(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn modern_quickplay_and_legacy_server_arguments_are_distinct() {
        let config = Config {
            server: "example.org:25566".into(),
            fullscreen: true,
            ..Config::default()
        };
        let meta = serde_json::json!({"arguments":{"game":[
            {"rules":[{"action":"allow","features":{"has_custom_resolution":true}}],"value":["--width","${resolution_width}","--height","${resolution_height}"]},
            {"rules":[{"action":"allow","features":{"has_quick_plays_support":true}}],"value":["--quickPlayPath","${quickPlayPath}"]},
            {"rules":[{"action":"allow","features":{"is_quick_play_multiplayer":true}}],"value":["--quickPlayMultiplayer","${quickPlayMultiplayer}"]},
            {"rules":[{"action":"allow","features":{"is_demo_user":true}}],"value":"--demo"}
        ]}});
        let args = game_arguments(&config, &meta).unwrap();
        assert_eq!(args.iter().filter(|arg| *arg == "--width").count(), 1);
        assert!(args.iter().any(|arg| arg == "--quickPlayMultiplayer"));
        assert!(args.iter().any(|arg| arg == "--fullscreen"));
        assert!(!args.iter().any(|arg| arg == "--server" || arg == "--demo"));
        let legacy = game_arguments(
            &config,
            &serde_json::json!({"minecraftArguments":"--username ${auth_player_name}"}),
        )
        .unwrap();
        assert!(legacy
            .windows(4)
            .any(|args| args == ["--server", "example.org", "--port", "25566"]));
        let plain = game_arguments(&Config::default(), &meta).unwrap();
        assert!(!plain.iter().any(|arg| arg.starts_with("--quickPlay")));
    }
}
