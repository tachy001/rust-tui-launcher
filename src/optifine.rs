use crate::{
    backend::Backend,
    config::Config,
    metadata::valid_id,
    optifine_archive::{self, Identity, OptiFineArchive},
    optifine_artifact_service::{Action, OptiFineArtifactService, Origin},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{collections::HashSet, fs, path::Path};

pub(crate) const CATALOG_ROOT: &str = "https://bmclapi2.bangbang93.com";
pub(crate) fn lookup_game(game: &str) -> &str {
    match game {
        "1.8" => "1.8.0",
        "1.9" => "1.9.0",
        game => game,
    }
}
fn normal_game(game: &str) -> &str {
    match game {
        "1.8.0" => "1.8",
        "1.9.0" => "1.9",
        game => game,
    }
}
#[derive(Clone)]
pub(crate) struct OptiFineVersion {
    pub identity: Identity,
    pub url: String,
    pub stable: bool,
}
pub(crate) fn catalog(bytes: &[u8], game: &str) -> Result<Vec<OptiFineVersion>> {
    let value: Value = serde_json::from_slice(bytes)?;
    let mut seen = HashSet::new();
    let mut result = vec![];
    for entry in value.as_array().context("OptiFine 版本目录必须是数组")? {
        let mc = entry["mcversion"]
            .as_str()
            .context("OptiFine 目录缺少 Minecraft 版本")?;
        if normal_game(mc) != normal_game(game) {
            bail!("OptiFine 目录返回其他 Minecraft 版本");
        }
        let identity = Identity {
            game: normal_game(mc).into(),
            edition: entry["type"]
                .as_str()
                .context("OptiFine 缺少 edition")?
                .into(),
            release: entry["patch"]
                .as_str()
                .context("OptiFine 缺少 release")?
                .into(),
        };
        identity.validate()?;
        if !seen.insert(identity.version()) {
            continue;
        }
        let release = identity.release.to_ascii_lowercase();
        let stable =
            !release.contains("pre") && !release.contains("alpha") && !release.contains("beta");
        let url = format!(
            "{CATALOG_ROOT}/optifine/{}/{}/{}",
            lookup_game(&identity.game),
            identity.edition,
            identity.release
        );
        result.push(OptiFineVersion {
            identity,
            url,
            stable,
        });
    }
    result.sort_by_cached_key(|version| {
        (
            !version.stable,
            std::cmp::Reverse(release_key(&version.identity.version())),
        )
    });
    Ok(result)
}
fn release_key(value: &str) -> Vec<(bool, usize, String)> {
    let mut result = vec![];
    let bytes = value.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let number = bytes[start].is_ascii_digit();
        let mut end = start + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() == number {
            end += 1;
        }
        let token = &value[start..end];
        let token = if number {
            let trimmed = token.trim_start_matches('0');
            if trimmed.is_empty() {
                "0"
            } else {
                trimmed
            }
        } else {
            token
        };
        result.push((
            number,
            if number { token.len() } else { 0 },
            token.to_ascii_lowercase(),
        ));
        start = end;
    }
    result
}
enum LaunchMode {
    Standalone,
    ForgeLegacy,
    ForgeModLauncher,
    ForgeBootstrap,
}
fn launch_mode(meta: &Value) -> Result<LaunchMode> {
    let main = meta["mainClass"].as_str().context("父实例没有启动入口")?;
    let libraries = meta["libraries"].as_array().context("父实例没有依赖列表")?;
    let forge = libraries
        .iter()
        .filter_map(|lib| lib["name"].as_str())
        .any(|name| {
            name.starts_with("net.minecraftforge:forge:")
                || name.starts_with("net.minecraftforge:minecraftforge:")
                || name.starts_with("net.minecraftforge:fmlloader:")
        });
    match main {
        "net.minecraft.client.main.Main" => Ok(LaunchMode::Standalone),
        "net.minecraft.launchwrapper.Launch" if forge => Ok(LaunchMode::ForgeLegacy),
        "cpw.mods.modlauncher.Launcher" if forge => Ok(LaunchMode::ForgeModLauncher),
        "cpw.mods.bootstraplauncher.BootstrapLauncher" if forge => Ok(LaunchMode::ForgeBootstrap),
        _ => bail!("当前父实例的启动入口不支持此 OptiFine 安装模式"),
    }
}
fn compatibility(meta: &Value, info: &OptiFineArchive) -> Result<LaunchMode> {
    let mode = launch_mode(meta)?;
    match mode {
        LaunchMode::Standalone if !info.tweaker => {
            bail!("此 OptiFine 包没有 Tweaker，替换客户端的旧格式尚未实现")
        }
        LaunchMode::ForgeLegacy if !info.forge_tweaker => {
            bail!("此 OptiFine 包没有 Forge Tweaker，不能用于所选旧 Forge")
        }
        LaunchMode::ForgeBootstrap
            if info
                .build
                .as_ref()
                .is_some_and(|build| build.as_str() < "20210924-190833") =>
        {
            bail!("此 OptiFine 构建不支持 BootstrapLauncher Forge")
        }
        _ => Ok(mode),
    }
}
pub(crate) fn profile(
    id: &str,
    parent: &str,
    meta: &Value,
    info: &OptiFineArchive,
    core: Value,
    wrapper: Option<Value>,
) -> Result<Value> {
    valid_id(id)?;
    valid_id(parent)?;
    if id == parent {
        bail!("OptiFine 实例 ID 不能覆盖父实例");
    }
    let mode = compatibility(meta, info)?;
    let mut libraries = vec![core];
    let mut args = json!({});
    let main = match mode {
        LaunchMode::Standalone | LaunchMode::ForgeLegacy => {
            let tweaker = if matches!(mode, LaunchMode::ForgeLegacy) {
                if !info.forge_tweaker {
                    bail!("此 OptiFine 包没有 Forge Tweaker，不能用于所选旧 Forge");
                }
                "optifine.OptiFineForgeTweaker"
            } else {
                if !info.tweaker {
                    bail!("此 OptiFine 包没有 Tweaker，替换客户端的旧格式尚未实现");
                }
                "optifine.OptiFineTweaker"
            };
            if let Some(legacy) = meta["minecraftArguments"].as_str() {
                args["minecraftArguments"] = format!("{legacy} --tweakClass {tweaker}").into();
            } else {
                args["arguments"] = json!({"game":["--tweakClass",tweaker]});
            }
            if matches!(mode, LaunchMode::Standalone) {
                libraries.push(
                    wrapper.unwrap_or_else(|| json!({"name":"net.minecraft:launchwrapper:1.12"})),
                );
            }
            "net.minecraft.launchwrapper.Launch"
        }
        LaunchMode::ForgeModLauncher => {
            libraries.push(crate::bundled_artifact::OPTIFINE_DISCOVERY.descriptor());
            let path = libraries[0]["downloads"]["artifact"]["path"]
                .as_str()
                .context("OptiFine 核心库缺少路径")?;
            args["arguments"] =
                json!({"jvm":[format!("-Dtui.optifine.jar=${{library_directory}}/{path}")]});
            "cpw.mods.modlauncher.Launcher"
        }
        LaunchMode::ForgeBootstrap => {
            if info
                .build
                .as_ref()
                .is_some_and(|build| build.as_str() < "20210924-190833")
            {
                bail!("此 OptiFine 构建不支持 BootstrapLauncher Forge");
            }
            "cpw.mods.bootstraplauncher.BootstrapLauncher"
        }
    };
    let mut result = json!({"id":id,"inheritsFrom":parent,"mainClass":main,"libraries":libraries,
        "tuiMcOptiFineVersion":format!("{}_{}",info.identity.game,info.identity.version())});
    for (key, value) in args.as_object().unwrap() {
        result[key] = value.clone();
    }
    Ok(result)
}
impl Backend {
    pub(crate) fn optifine_versions(&self, game: &str) -> Result<Vec<OptiFineVersion>> {
        valid_id(game)?;
        self.cached_metadata(
            &format!("{CATALOG_ROOT}/optifine/{}", lookup_game(game)),
            |bytes| catalog(bytes, game),
        )
    }
    pub(crate) fn install_optifine(
        &self,
        config: &Config,
        game: &str,
        requested: Option<&str>,
    ) -> Result<String> {
        let versions = self.optifine_versions(game)?;
        let chosen = if let Some(requested) = requested {
            versions
                .iter()
                .find(|version| version.identity.version() == requested)
        } else {
            versions.first()
        }
        .context("没有匹配的 OptiFine 版本")?;
        let remote = self.remote_version(game)?;
        self.install(config, &remote)?;
        let root = config.root()?;
        let path = root.join("tui-mc-optifine/downloads").join(format!(
            "{}_{}-installer.jar",
            game,
            chosen.identity.version()
        ));
        self.downloader().fetch(&crate::download::Download {
            url: chosen.url.clone(),
            dest: path.clone(),
            sha1: None,
            size: None,
            alternatives: vec![],
        })?;
        let info = OptiFineArchive::inspect(&path, &self.control)?;
        if info.identity.game != chosen.identity.game
            || info.identity.version() != chosen.identity.version()
        {
            bail!("下载的 OptiFine 身份与所选版本不符，未运行补丁器");
        }
        self.install_optifine_local_source(config, game, &path, None, Some(chosen.url.clone()))
    }
    pub(crate) fn install_optifine_local(
        &self,
        config: &Config,
        parent: &str,
        path: &Path,
        id: Option<&str>,
    ) -> Result<String> {
        self.install_optifine_local_source(config, parent, path, id, None)
    }
    fn install_optifine_local_source(
        &self,
        config: &Config,
        parent: &str,
        path: &Path,
        id: Option<&str>,
        url: Option<String>,
    ) -> Result<String> {
        use crate::game_java_service::ClientVersionSource;
        self.control.check()?;
        valid_id(parent)?;
        let info = OptiFineArchive::inspect(path, &self.control)?;
        let default = format!("{parent}-OptiFine-{}", info.identity.version());
        let id = id.unwrap_or(&default);
        valid_id(id)?;
        let root = config.root()?;
        let mut mutation = crate::instance_service::InstanceMutation::begin(&root, id)?;
        let resolved = mutation.resolve(parent)?;
        compatibility(&resolved.meta, &info)?;
        let client = root
            .join("versions")
            .join(&resolved.jar_id)
            .join(format!("{}.jar", resolved.jar_id));
        let client_info =
            crate::game_java_service::JarClientVersionSource.read(&client, &self.control)?;
        let base = client_info
            .version
            .as_deref()
            .or_else(|| resolved.chain.first().map(String::as_str))
            .context("无法识别 OptiFine 的基础游戏版本")?;
        if normal_game(base) != normal_game(&info.identity.game) {
            bail!(
                "OptiFine 安装包适用于 {}，父实例为 {base}",
                info.identity.game
            );
        }
        if let Some(hash) = resolved.meta["downloads"]["client"]["sha1"].as_str() {
            if !crate::download::valid_file(&client, Some(hash), None)? {
                bail!("原版客户端校验失败，请先修复父实例");
            }
        }
        let game = crate::game_java_service::GameJavaResolver::new(
            &crate::game_java_service::JarClientVersionSource,
        )
        .resolve(&root, &resolved, &self.control)?;
        let raw_sha1 = optifine_archive::digest(path, &self.control)?;
        let client_sha1 = optifine_archive::digest(&client, &self.control)?;
        let mut origin = Origin {
            schema: 1,
            raw_sha1,
            raw_url: url,
            jar_id: resolved.jar_id.clone(),
            client_sha1,
            game_minimum: game.requirement.recommended(),
            game: info.identity.game.clone(),
            edition: info.identity.edition.clone(),
            release: info.identity.release.clone(),
            action: if info.patcher {
                Action::Patch
            } else {
                Action::Copy
            },
        };
        origin.validate()?;
        let raw = origin.raw_path(&root);
        {
            let _lease = crate::download_lock::DownloadLease::acquire(&raw, &self.control, || {})?;
            if !crate::download::valid_file(&raw, Some(&origin.raw_sha1), None)? {
                crate::storage::atomic_copy(&raw, &mut fs::File::open(path)?)?;
            }
        }
        let effective = config.effective(parent)?;
        let artifacts = OptiFineArtifactService::new(self, &effective, &self.control);
        let suffix = format!("-tui.{}.{}", origin.raw_sha1, origin.client_sha1);
        let coordinate = format!(
            "optifine:OptiFine:{}_{}{suffix}",
            info.identity.game,
            info.identity.version()
        );
        let core = artifacts.library(&root, &coordinate, origin.clone())?;
        let wrapper = if matches!(launch_mode(&resolved.meta)?, LaunchMode::Standalone) {
            if let Some(wrapper) = &info.wrapper {
                origin.action = Action::Embedded {
                    entry: wrapper.entry.clone(),
                };
                Some(artifacts.library(
                    &root,
                    &format!("optifine:{}:{}{suffix}", wrapper.artifact, wrapper.version),
                    origin,
                )?)
            } else {
                None
            }
        } else {
            None
        };
        let child = profile(id, parent, &resolved.meta, &info, core, wrapper)?;
        self.install_profile_with_session(config, id, child, &mut mutation)?;
        self.log(format!("OptiFine {} 已安装：{id}", info.identity.version()));
        Ok(id.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{optifine_archive::fixtures, task::TaskControl};
    use sha1::{Digest, Sha1};
    #[test]
    fn catalog_normalizes_lookup_versions_and_keeps_release_channels_distinct() {
        let bytes = serde_json::to_vec(&json!([
            {"mcversion":"1.8.0","type":"HD_U","patch":"I6_pre2"},
            {"mcversion":"1.8.0","type":"HD_U","patch":"I5"},
            {"mcversion":"1.8.0","type":"HD_U","patch":"I5"}
        ]))
        .unwrap();
        let versions = catalog(&bytes, "1.8").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].identity.version(), "HD_U_I5");
        assert!(versions[0].stable);
        assert!(!versions[1].stable);
        assert!(versions[0].url.contains("/1.8.0/HD_U/I5"));
        assert!(catalog(&bytes, "1.12.2").is_err());
        assert!(catalog(
            br#"[{"mcversion":"1.8","type":"HD_U","patch":"../escape"}]"#,
            "1.8"
        )
        .is_err());
        assert!(catalog(br#"{"error":"unavailable"}"#, "1.8").is_err());
    }
    #[test]
    fn preview_api_field_partition_and_numeric_release_order_are_supported() {
        let versions = catalog(
            &serde_json::to_vec(&json!([
                {"mcversion":"1.21.1","type":"HD_U","patch":"I9"},
                {"mcversion":"1.21.1","type":"HD_U","patch":"I10"},
                {"mcversion":"1.21.1","type":"HD_U_J1","patch":"pre9"},
                {"mcversion":"1.21.1","type":"HD_U_J1","patch":"pre10"}
            ]))
            .unwrap(),
            "1.21.1",
        )
        .unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|v| v.identity.version())
                .collect::<Vec<_>>(),
            ["HD_U_I10", "HD_U_I9", "HD_U_J1_pre10", "HD_U_J1_pre9"]
        );
        assert!(versions[2].url.ends_with("/HD_U_J1/pre10"));
        let raw = Identity {
            game: "1.21.1".into(),
            edition: "HD_U".into(),
            release: "J1_pre10".into(),
        };
        assert_eq!(versions[2].identity.version(), raw.version());
    }
    fn info() -> OptiFineArchive {
        OptiFineArchive {
            identity: Identity {
                game: "1.12.2".into(),
                edition: "HD_U".into(),
                release: "I6".into(),
            },
            patcher: true,
            wrapper: None,
            build: Some("20211001-000000".into()),
            tweaker: true,
            forge_tweaker: true,
        }
    }
    fn core() -> Value {
        json!({"name":"optifine:OptiFine:test","downloads":{"artifact":{"path":"optifine/OptiFine/test/OptiFine-test.jar","url":"https://fixture.invalid/of.jar"}}})
    }
    #[test]
    fn standalone_preserves_auth_arguments_in_legacy_and_modern_profiles() {
        let legacy = json!({"mainClass":"net.minecraft.client.main.Main","minecraftArguments":"--username ${auth_player_name}","libraries":[]});
        let child = profile("of", "base", &legacy, &info(), core(), None).unwrap();
        let merged = crate::version::merge(&legacy, &child);
        assert_eq!(
            merged["minecraftArguments"],
            "--username ${auth_player_name} --tweakClass optifine.OptiFineTweaker"
        );
        let modern = json!({"mainClass":"net.minecraft.client.main.Main","arguments":{"game":["--username","${auth_player_name}"],"jvm":["-cp","${classpath}"]},"libraries":[]});
        let wrapper = json!({"name":"optifine:launchwrapper-of:2.3"});
        let child = profile("of", "base", &modern, &info(), core(), Some(wrapper)).unwrap();
        let merged = crate::version::merge(&modern, &child);
        assert_eq!(
            crate::metadata::arguments(&merged["arguments"]["game"]),
            [
                "--username",
                "${auth_player_name}",
                "--tweakClass",
                "optifine.OptiFineTweaker"
            ]
        );
        assert_eq!(
            child["libraries"][1]["name"],
            "optifine:launchwrapper-of:2.3"
        );
        assert_eq!(child["mainClass"], "net.minecraft.launchwrapper.Launch");
    }
    #[test]
    fn forge_modes_keep_their_entry_points_and_apply_only_the_required_optifine_hooks() {
        let library = json!({"name":"net.minecraftforge:forge:1.12.2-14.23.5.2859"});
        let legacy = json!({"mainClass":"net.minecraft.launchwrapper.Launch","minecraftArguments":"--tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker","libraries":[library]});
        let child = profile("of", "forge", &legacy, &info(), core(), None).unwrap();
        assert!(child["minecraftArguments"]
            .as_str()
            .unwrap()
            .contains("OptiFineForgeTweaker"));
        assert!(child["minecraftArguments"]
            .as_str()
            .unwrap()
            .contains("FMLTweaker"));
        assert_eq!(child["libraries"].as_array().unwrap().len(), 1);
        for main in [
            "cpw.mods.modlauncher.Launcher",
            "cpw.mods.bootstraplauncher.BootstrapLauncher",
        ] {
            let parent = json!({"mainClass":main,"libraries":[library]});
            let child = profile("of", "forge", &parent, &info(), core(), None).unwrap();
            assert_eq!(child["mainClass"], main);
            assert!(child.pointer("/arguments/game").is_none());
            if main.contains("modlauncher") {
                assert_eq!(
                    child["libraries"][1]["name"],
                    "app.tuilauncher:optifine-discovery:1.0"
                );
                assert!(child["arguments"]["jvm"][0]
                    .as_str()
                    .unwrap()
                    .contains("${library_directory}/optifine/"));
            } else {
                assert_eq!(child["libraries"].as_array().unwrap().len(), 1);
            }
        }
    }
    #[test]
    fn incompatible_loaders_old_bootstrap_builds_and_parent_overwrite_are_rejected() {
        let fabric =
            json!({"mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[]});
        assert!(profile("of", "base", &fabric, &info(), core(), None).is_err());
        let bootstrap = json!({"mainClass":"cpw.mods.bootstraplauncher.BootstrapLauncher","libraries":[{"name":"net.minecraftforge:forge:1.17.1-37.0.1"}]});
        let mut old = info();
        old.build = Some("20210101-000000".into());
        assert!(profile("of", "base", &bootstrap, &old, core(), None).is_err());
        let vanilla = json!({"mainClass":"net.minecraft.client.main.Main","libraries":[]});
        assert!(profile("base", "base", &vanilla, &info(), core(), None).is_err());
    }
    fn backend() -> Backend {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut backend = Backend::new(tx).unwrap();
        backend.client = reqwest::blocking::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();
        backend
    }
    #[test]
    fn backend_local_install_and_repair_publish_only_after_preparing_the_derived_libraries() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            java: "must-not-run-java".into(),
            ..Default::default()
        };
        let root = config.root().unwrap();
        let base = root.join("versions/1.12.2");
        fs::create_dir_all(&base).unwrap();
        let client = fixtures::jar(&[("version.json", br#"{"id":"1.12.2"}"#)]);
        fs::write(base.join("1.12.2.jar"), &client).unwrap();
        let assets = br#"{"objects":{}}"#;
        fs::create_dir_all(root.join("assets/indexes")).unwrap();
        fs::write(root.join("assets/indexes/fixture.json"), assets).unwrap();
        let meta = json!({"id":"1.12.2","mainClass":"net.minecraft.client.main.Main","javaVersion":{"majorVersion":8},"minecraftArguments":"--username ${auth_player_name}","libraries":[],
            "downloads":{"client":{"url":"https://fixture.invalid/client.jar","sha1":format!("{:x}",Sha1::digest(&client)),"size":client.len()}},
            "assetIndex":{"id":"fixture","url":"https://fixture.invalid/assets.json","sha1":format!("{:x}",Sha1::digest(assets)),"size":assets.len()}});
        fs::write(base.join("1.12.2.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        let raw = root.join("local-of.jar");
        fixtures::installer(&raw, "1.12.2", false, true);
        let backend = backend();
        assert_eq!(
            backend
                .install_optifine_local(&config, "1.12.2", &raw, Some("of"))
                .unwrap(),
            "of"
        );
        let profile_path = root.join("versions/of/of.json");
        let previous = fs::read(&profile_path).unwrap();
        let profile: Value = serde_json::from_slice(&previous).unwrap();
        let resolved = crate::version::resolve(&root, "of").unwrap();
        assert_eq!(resolved.jar_id, "1.12.2");
        assert_eq!(
            resolved.meta["mainClass"],
            "net.minecraft.launchwrapper.Launch"
        );
        let artifact = &profile["libraries"][0];
        let path = root
            .join("libraries")
            .join(artifact["downloads"]["artifact"]["path"].as_str().unwrap());
        let original = fs::read(&path).unwrap();
        fs::write(&path, b"corrupt").unwrap();
        backend.repair(&config, "of").unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read(&profile_path).unwrap(), previous);
        let origin: Origin = serde_json::from_value(artifact["tuiMcOptiFine"].clone()).unwrap();
        fs::remove_file(origin.raw_path(&root)).unwrap();
        backend.repair(&config, "of").unwrap();
        fs::write(&path, b"preserve damaged content").unwrap();
        assert!(backend.repair(&config, "of").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"preserve damaged content");
        assert_eq!(fs::read(&profile_path).unwrap(), previous);
        backend
            .install_optifine_local(&config, "1.12.2", &raw, Some("of"))
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!serde_json::to_string(artifact)
            .unwrap()
            .contains(root.to_string_lossy().as_ref()));
    }
    #[test]
    fn local_identity_mismatch_is_rejected_before_creating_work_or_publishing_a_child() {
        let workspace = crate::test_support::Workspace::new();
        let config = Config {
            game_dir: workspace.path.clone(),
            java: "must-not-run-java".into(),
            ..Default::default()
        };
        let root = config.root().unwrap();
        let base = root.join("versions/1.12.2");
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("1.12.2.jar"),
            fixtures::jar(&[("version.json", br#"{"id":"1.12.2"}"#)]),
        )
        .unwrap();
        fs::write(
            base.join("1.12.2.json"),
            serde_json::to_vec(
                &json!({"id":"1.12.2","mainClass":"net.minecraft.client.main.Main","libraries":[]}),
            )
            .unwrap(),
        )
        .unwrap();
        let raw = root.join("other.jar");
        fixtures::installer(&raw, "1.20.1", true, false);
        assert!(backend()
            .install_optifine_local(&config, "1.12.2", &raw, Some("of"))
            .is_err());
        assert!(!root.join("tui-mc-optifine/work").exists());
        assert!(!root.join("versions/of/of.json").exists());
        assert!(OptiFineArchive::inspect(&raw, &TaskControl::default()).is_ok());
    }
}
