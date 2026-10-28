use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;

pub struct ServerAddress {
    pub host: String,
    pub port: u16,
}
pub(crate) enum QuickPlayTarget {
    None,
    Multiplayer(ServerAddress),
    Singleplayer(String),
}
fn has_option(arguments: &[String], option: &str) -> bool {
    arguments.iter().any(|argument| {
        argument == option
            || argument
                .strip_prefix(option)
                .is_some_and(|suffix| suffix.starts_with('='))
    })
}
impl QuickPlayTarget {
    pub(crate) fn from_config(config: &Config) -> Result<Self> {
        let server = server_address(&config.server)?;
        if !config.quick_play_world.is_empty() {
            crate::metadata::valid_id(&config.quick_play_world)?;
            if server.is_some() {
                bail!("不能同时快速进入服务器和本地存档，请只选择一个目标");
            }
            Ok(Self::Singleplayer(config.quick_play_world.clone()))
        } else {
            Ok(server.map(Self::Multiplayer).unwrap_or(Self::None))
        }
    }
    pub(crate) fn validate_world(&self, game_dir: &std::path::Path) -> Result<()> {
        if let Self::Singleplayer(name) = self {
            crate::worlds::require_world(game_dir, name)?;
        }
        Ok(())
    }
    pub(crate) fn features(
        &self,
        meta: &Value,
        platform: &crate::platform_rule_service::PlatformRuleEvaluator<'_>,
    ) -> Result<HashMap<&'static str, bool>> {
        let mut features = HashMap::from([
            ("has_custom_resolution", true),
            ("has_quick_plays_support", !matches!(self, Self::None)),
            (
                "is_quick_play_multiplayer",
                matches!(self, Self::Multiplayer(_)),
            ),
            (
                "is_quick_play_singleplayer",
                matches!(self, Self::Singleplayer(_)),
            ),
            ("is_quick_play_realms", false),
        ]);
        let candidate =
            crate::metadata::arguments_for(&meta["arguments"]["game"], &features, platform);
        let option = match self {
            Self::Multiplayer(_) => Some("--quickPlayMultiplayer"),
            Self::Singleplayer(_) => Some("--quickPlaySingleplayer"),
            Self::None => None,
        };
        let supported = option.is_some_and(|option| has_option(&candidate, option));
        if matches!(self, Self::Singleplayer(_)) && !supported {
            bail!("此版本未声明可用的单人 Quick Play 参数，请从游戏内打开存档");
        }
        if !supported {
            features.insert("has_quick_plays_support", false);
            features.insert("is_quick_play_multiplayer", false);
            features.insert("is_quick_play_singleplayer", false);
        }
        Ok(features)
    }
}
/// Expand only the original template; substituted paths and tokens are literal data.
pub fn expand_argument(template: &str, values: &HashMap<&str, String>) -> Result<String> {
    let mut output = String::new();
    let mut remaining = template;
    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let suffix = &remaining[start + 2..];
        let end = suffix.find('}').context("启动参数占位符缺少右花括号")?;
        let value = values
            .get(&suffix[..end])
            .context("启动参数中存在未识别的占位符，请检查版本元数据或自定义参数")?;
        output.push_str(value);
        remaining = &suffix[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
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

#[cfg(test)]
pub fn game_arguments(config: &Config, meta: &Value) -> Result<Vec<String>> {
    game_arguments_for(
        config,
        meta,
        &crate::platform_rule_service::PlatformRuleEvaluator::current(),
    )
}
pub(crate) fn game_arguments_for(
    config: &Config,
    meta: &Value,
    platform: &crate::platform_rule_service::PlatformRuleEvaluator<'_>,
) -> Result<Vec<String>> {
    let target = QuickPlayTarget::from_config(config)?;
    let features = target.features(meta, platform)?;
    let mut args = if let Some(legacy) = meta["minecraftArguments"].as_str() {
        shlex::split(legacy).context("无法解析旧版启动参数")?
    } else {
        Vec::new()
    };
    args.extend(crate::metadata::arguments_for(
        &meta["arguments"]["game"],
        &features,
        platform,
    ));
    if !has_option(&args, "--width") {
        args.extend(["--width".into(), config.width.to_string()]);
    }
    if !has_option(&args, "--height") {
        args.extend(["--height".into(), config.height.to_string()]);
    }
    if config.fullscreen && !has_option(&args, "--fullscreen") {
        args.push("--fullscreen".into());
    }
    if let QuickPlayTarget::Multiplayer(server) = target {
        if features["is_quick_play_multiplayer"] {
            if !has_option(&args, "--quickPlayMultiplayer") {
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
    fn expansion_keeps_inserted_values_literal() {
        let values = HashMap::from([
            ("game_directory", r"C:\Games\${profile}\中文".into()),
            ("auth_access_token", "${game_directory}".into()),
        ]);
        assert_eq!(
            expand_argument("${game_directory}/${auth_access_token}", &values).unwrap(),
            r"C:\Games\${profile}\中文/${game_directory}"
        );
        assert!(expand_argument("${unknown}", &values).is_err());
        assert!(expand_argument("${unfinished", &values).is_err());
        assert_eq!(expand_argument("plain", &values).unwrap(), "plain");
    }
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
    fn mixed_argument_formats_keep_loader_and_quickplay_options() {
        let meta = serde_json::json!({"minecraftArguments":"--username ${auth_player_name}","arguments":{"game":["--tweakClass","example.Loader",
            {"rules":[{"action":"allow","features":{"is_quick_play_multiplayer":true}}],"value":["--quickPlayMultiplayer","${quickPlayMultiplayer}"]}]}});
        let config = Config {
            server: "example.org".into(),
            ..Config::default()
        };
        let args = game_arguments(&config, &meta).unwrap();
        assert_eq!(
            &args[..4],
            [
                "--username",
                "${auth_player_name}",
                "--tweakClass",
                "example.Loader"
            ]
        );
        assert!(args.iter().any(|arg| arg == "--quickPlayMultiplayer"));
        assert!(!args.iter().any(|arg| arg == "--server"));
        assert_eq!(args.iter().filter(|arg| *arg == "--width").count(), 1);
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

    #[test]
    fn singleplayer_quickplay_selects_only_world_and_expands_folder_as_literal_data() {
        let config = Config {
            quick_play_world: "中文 World ${slot}".into(),
            ..Config::default()
        };
        let meta = serde_json::json!({"arguments":{"game":[
            {"rules":[{"action":"allow","features":{"has_quick_plays_support":true}}],"value":["--quickPlayPath","${quickPlayPath}"]},
            {"rules":[{"action":"allow","features":{"is_quick_play_singleplayer":true}}],"value":["--quickPlaySingleplayer","${quickPlaySingleplayer}"]},
            {"rules":[{"action":"allow","features":{"is_quick_play_multiplayer":true}}],"value":["--quickPlayMultiplayer","${quickPlayMultiplayer}"]}
        ]}});
        let args = game_arguments(&config, &meta).unwrap();
        assert!(args.contains(&"--quickPlaySingleplayer".into()));
        assert!(!args.contains(&"--quickPlayMultiplayer".into()));
        let values = HashMap::from([("quickPlaySingleplayer", config.quick_play_world.clone())]);
        assert_eq!(
            expand_argument("${quickPlaySingleplayer}", &values).unwrap(),
            config.quick_play_world
        );
        assert!(game_arguments(
            &config,
            &serde_json::json!({"minecraftArguments":"--username Player"})
        )
        .is_err());
        let conflict = Config {
            server: "example.org".into(),
            ..config
        };
        assert!(QuickPlayTarget::from_config(&conflict).is_err());
    }

    #[test]
    fn disallowed_or_inapplicable_quickplay_does_not_enable_modern_server_flags() {
        let config = Config {
            server: "example.org".into(),
            ..Config::default()
        };
        for rules in [
            serde_json::json!([{"action":"disallow","features":{"is_quick_play_multiplayer":true}}]),
            serde_json::json!([{"action":"allow","os":{"name":"unknown-nonexistent"},"features":{"is_quick_play_multiplayer":true}}]),
        ] {
            let meta = serde_json::json!({"arguments":{"game":[{"rules":rules,"value":["--quickPlayMultiplayer","${quickPlayMultiplayer}"]}]}});
            let args = game_arguments(&config, &meta).unwrap();
            assert!(!args.contains(&"--quickPlayMultiplayer".into()));
            assert!(args.contains(&"--server".into()));
        }
        let path_only = serde_json::json!({"arguments":{"game":[{"rules":[{"action":"allow","features":{"has_quick_plays_support":true}}],"value":["--quickPlayPath","${quickPlayPath}"]}]}});
        let args = game_arguments(&config, &path_only).unwrap();
        assert!(args.contains(&"--server".into()));
        assert!(!args.contains(&"--quickPlayPath".into()));
    }

    #[test]
    fn equals_form_window_arguments_are_not_duplicated() {
        let meta = serde_json::json!({"arguments":{"game":["--width=1280","--height=720","--fullscreen=true"]}});
        let args = game_arguments(
            &Config {
                fullscreen: true,
                ..Config::default()
            },
            &meta,
        )
        .unwrap();
        assert_eq!(args, ["--width=1280", "--height=720", "--fullscreen=true"]);
    }
}
