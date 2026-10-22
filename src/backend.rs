pub(crate) use crate::process_service::ManagedProcess as GameProcess;
use crate::{
    config::Config,
    download::{Download, Downloader},
    metadata::*,
    process_service::{ProcessMonitor, ProcessOutcome},
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use md5::Md5;
use reqwest::blocking::Client;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::Sender,
    time::Duration,
};

#[derive(Debug)]
pub enum Message {
    Log(String),
    Progress(String, usize, usize),
    Transfer(crate::download_progress::Snapshot),
    Versions(Vec<RemoteVersion>),
    LoaderVersions {
        kind: crate::loader_catalog::InstallerKind,
        game: String,
        versions: Vec<crate::loader_catalog::LoaderVersion>,
    },
    Done(Result<String, String>),
    Pauseable(bool),
    ModSearch {
        instance: String,
        query: String,
        result: crate::modrinth::SearchResult,
        updates: bool,
    },
    ModVersions {
        instance: String,
        project: String,
        versions: Vec<crate::modrinth::ModVersion>,
    },
}
fn download_entry(entry: &Value, dest: &Path) -> Result<Download> {
    Ok(Download {
        url: entry["url"].as_str().context("资源缺少下载地址")?.into(),
        dest: dest.into(),
        sha1: entry["sha1"].as_str().map(str::to_owned),
        size: entry["size"].as_u64(),
        alternatives: vec![],
    })
}
pub struct Backend {
    pub(crate) client: Client,
    pub tx: Sender<Message>,
    pub control: TaskControl,
    pub source: String,
}
impl Backend {
    pub fn new(tx: Sender<Message>) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(90))
                .user_agent(concat!(
                    env!("CARGO_PKG_NAME"),
                    "/",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()?,
            tx,
            control: TaskControl::default(),
            source: "official".into(),
        })
    }
    pub fn log(&self, s: impl Into<String>) {
        let _ = self.tx.send(Message::Log(s.into()));
    }
    pub fn catalog(&self) -> Result<()> {
        let manifest = self.manifest()?;
        let _ = self.tx.send(Message::Versions(manifest.versions));
        Ok(())
    }
    pub fn get_json(&self, url: &str) -> Result<Value> {
        self.control.check()?;
        if !url.starts_with("https://") {
            bail!("API 必须使用 HTTPS");
        }
        let mut last = None;
        for candidate in crate::sources::urls(url, &self.source) {
            self.control.check()?;
            let response = (|| -> Result<Value> {
                Ok(self
                    .client
                    .get(candidate)
                    .send()?
                    .error_for_status()?
                    .json()?)
            })();
            match response {
                Ok(value) => {
                    self.control.check()?;
                    return Ok(value);
                }
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("无法读取远程元数据")))
    }
    pub(crate) fn downloader(&self) -> Downloader {
        Downloader {
            client: self.client.clone(),
            control: self.control.clone(),
            tx: self.tx.clone(),
            source: self.source.clone(),
        }
    }
    fn fetch(&self, url: &str, path: &Path, expected: Option<&str>) -> Result<()> {
        self.downloader().fetch(&Download {
            url: url.into(),
            dest: path.into(),
            sha1: expected.map(str::to_owned),
            size: None,
            alternatives: vec![],
        })
    }
    fn download_entry(&self, entry: &Value, dest: &Path) -> Result<()> {
        self.downloader().fetch(&download_entry(entry, dest)?)
    }
    pub fn install(&self, config: &Config, remote: &RemoteVersion) -> Result<()> {
        valid_id(&remote.id)?;
        let root = config.root()?;
        self.control.check()?;
        let mut mutation = crate::instance_service::InstanceMutation::begin(&root, &remote.id)?;
        self.log(format!("读取版本 {}", remote.id));
        if remote.sha1.len() != 40 || !remote.sha1.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("版本元数据缺少有效 SHA-1");
        }
        let json_path = root
            .join("tui-mc-install-manifests")
            .join(format!("{}.json", remote.sha1.to_ascii_lowercase()));
        let installed_json = root
            .join("versions")
            .join(&remote.id)
            .join(format!("{}.json", remote.id));
        if !crate::download::valid_file(&json_path, Some(&remote.sha1), None)?
            && crate::download::valid_file(&installed_json, Some(&remote.sha1), None)?
        {
            crate::storage::atomic_copy(&json_path, &mut fs::File::open(installed_json)?)?;
        }
        self.fetch(&remote.url, &json_path, Some(&remote.sha1))?;
        let profile: Value =
            serde_json::from_slice(&fs::read(json_path)?).context("版本元数据格式错误")?;
        self.install_profile_with_session(config, &remote.id, profile, &mut mutation)
    }
    pub fn repair(&self, config: &Config, id: &str) -> Result<()> {
        self.control.check()?;
        let mut mutation = crate::instance_service::InstanceMutation::begin(&config.root()?, id)?;
        self.repair_with_session(config, id, &mut mutation)
    }
    pub(crate) fn repair_with_session(
        &self,
        config: &Config,
        id: &str,
        mutation: &mut crate::instance_service::InstanceMutation,
    ) -> Result<()> {
        self.control.check()?;
        let resolved = mutation.resolve(id)?;
        for message in mutation.take_recovery_messages() {
            self.log(message);
        }
        self.repair_resolved_with_session(config, id, mutation, &resolved, |entry, dest| {
            self.download_entry(entry, dest)
        })?;
        self.log("安装完成，可选择此版本启动");
        Ok(())
    }
    pub(crate) fn install_profile_with_session(
        &self,
        config: &Config,
        id: &str,
        profile: Value,
        mutation: &mut crate::instance_service::InstanceMutation,
    ) -> Result<()> {
        let root = config.root()?;
        let mut draft = crate::instance_draft::InstanceDraft::begin(
            &root,
            id,
            profile,
            mutation,
            &self.control,
        )?;
        let resolved = mutation.resolve_candidate(draft.profile())?;
        for message in mutation.take_recovery_messages() {
            self.log(message);
        }
        if !resolved.meta["mainClass"].as_str().is_some_and(|main| {
            !main.is_empty() && !main.chars().any(|c| c.is_control() || c.is_whitespace())
        }) {
            bail!("安装草稿缺少有效游戏启动入口，正式配置未修改");
        }
        let config = config.with_profile(id, draft.profile());
        self.repair_resolved_with_session(&config, id, mutation, &resolved, |entry, dest| {
            draft.prepare_client(
                &download_entry(entry, dest)?,
                |entry| self.downloader().fetch(entry),
                mutation,
                &self.control,
            )
        })?;
        if let Some(warning) = draft.commit(mutation, &self.control)? {
            self.log(warning);
        }
        self.log("安装完成，可选择此版本启动");
        Ok(())
    }
    fn repair_resolved_with_session(
        &self,
        config: &Config,
        id: &str,
        mutation: &crate::instance_service::InstanceMutation,
        resolved: &crate::version::ResolvedVersion,
        prepare_client: impl FnOnce(&Value, &Path) -> Result<()>,
    ) -> Result<()> {
        self.control.check()?;
        let config = config.effective(id)?;
        config.validate()?;
        let root = config.root()?;
        mutation.require(&root, id)?;
        let meta = &resolved.meta;
        self.log("校验客户端与依赖库");
        prepare_client(
            &meta["downloads"]["client"],
            &root
                .join("versions")
                .join(&resolved.jar_id)
                .join(format!("{}.jar", resolved.jar_id)),
        )?;
        let libs = meta["libraries"].as_array().context("版本缺少 libraries")?;
        let mut downloads = vec![];
        for lib in libs {
            if !allowed(lib) {
                continue;
            }
            crate::bundled_artifact::OPTIFINE_DISCOVERY.prepare(&root, lib, &self.control)?;
            crate::forge_installer_service::InstallerArtifactService::new(self, &self.control)
                .prepare(&root, lib)?;
            crate::optifine_artifact_service::OptiFineArtifactService::new(
                self,
                &config,
                &self.control,
            )
            .prepare(&root, lib, Some(&resolved.jar_id))?;
            for (entry, _) in artifacts(lib)? {
                downloads.push(download_entry(
                    &entry,
                    &safe_path(
                        &root.join("libraries"),
                        entry["path"].as_str().context("库路径缺失")?,
                    )?,
                )?);
            }
        }
        self.downloader()
            .many(downloads, config.download_threads, "依赖库")?;
        let index = &meta["assetIndex"];
        let index_id = index["id"].as_str().context("缺少资源索引 ID")?;
        valid_id(index_id)?;
        let index_path = root.join("assets/indexes").join(format!("{index_id}.json"));
        self.download_entry(index, &index_path)?;
        let assets: Value = serde_json::from_slice(&fs::read(index_path)?)?;
        let objects = assets["objects"]
            .as_object()
            .context("资源索引无 objects")?;
        let mut downloads = vec![];
        for object in objects.values() {
            let hash = object["hash"].as_str().context("资源缺少 hash")?;
            if hash.len() != 40 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                bail!("无效资源 hash");
            }
            let relative = format!("{}/{hash}", &hash[..2]);
            let dest = root.join("assets/objects").join(&relative);
            downloads.push(Download {
                url: format!("https://resources.download.minecraft.net/{relative}"),
                dest,
                sha1: Some(hash.into()),
                size: object["size"].as_u64(),
                alternatives: vec![],
            });
        }
        self.downloader()
            .many(downloads, config.download_threads, "游戏资源")?;
        let game_dir = config.instance_dir(id)?;
        crate::asset_layout::reconstruct(&root, &game_dir, index_id, &assets, &self.control)?;
        if let Some(logging) = meta.pointer("/logging/client") {
            let file = &logging["file"];
            let name = file["id"].as_str().context("日志配置缺少 ID")?;
            self.download_entry(file, &safe_path(&root.join("assets/log_configs"), name)?)?;
        }
        self.repair_pack_files_with_session(&config, id, mutation)?;
        self.repair_mod_inventory_with_session(&config, id, mutation)?;
        Ok(())
    }
    fn prepare_launch_libraries<'a>(
        &self,
        root: &Path,
        meta: &'a Value,
        platform: &crate::launch_platform::LaunchPlatform,
        threads: usize,
        context: Option<(&Config, &str)>,
    ) -> Result<Vec<(std::path::PathBuf, bool, &'a Value)>> {
        self.control.check()?;
        let resolver = platform.libraries();
        let rules = platform.rules();
        let mut libraries = vec![];
        let mut downloads = vec![];
        for lib in meta["libraries"].as_array().context("缺少 libraries")? {
            if !rules.allowed(lib, &HashMap::new()) {
                continue;
            }
            crate::bundled_artifact::OPTIFINE_DISCOVERY.prepare(root, lib, &self.control)?;
            crate::forge_installer_service::InstallerArtifactService::new(self, &self.control)
                .prepare(root, lib)?;
            if lib.get("tuiMcOptiFine").is_some() {
                let (config, owner) = context.context("OptiFine 启动依赖缺少客户端上下文")?;
                crate::optifine_artifact_service::OptiFineArtifactService::new(
                    self,
                    config,
                    &self.control,
                )
                .prepare(root, lib, Some(owner))?;
            }
            for (entry, native) in resolver.resolve(lib)? {
                let path = safe_path(
                    &root.join("libraries"),
                    entry["path"].as_str().context("缺少库路径")?,
                )?;
                downloads.push(download_entry(&entry, &path)?);
                libraries.push((path, native, lib));
            }
        }
        self.downloader().many(downloads, threads, "启动依赖")?;
        Ok(libraries)
    }
    pub fn launch(&self, config: &Config, id: &str) -> Result<()> {
        valid_id(id)?;
        config.validate()?;
        let root = config.root()?;
        let mut running = crate::instance_service::InstanceLaunchSession::begin(&root, id)?;
        let resolved = running.resolve(id)?;
        for message in running.messages() {
            self.log(message);
        }
        let config = config.effective(id)?;
        config.validate()?;
        let version_dir = root.join("versions").join(id);
        let meta = &resolved.meta;
        self.log(format!("版本链：{}", resolved.chain.join(" → ")));
        let jar = root
            .join("versions")
            .join(&resolved.jar_id)
            .join(format!("{}.jar", resolved.jar_id));
        if !jar.is_file() {
            bail!("客户端缺失，请先安装");
        }
        if let Some(hash) = meta["downloads"]["client"]["sha1"].as_str() {
            if !verify(&jar, hash)? {
                bail!("客户端校验失败，请重新安装");
            }
        }
        let game_dir = config.instance_dir(id)?;
        let plan = crate::game_java_service::GameJavaResolver::new(
            &crate::game_java_service::JarClientVersionSource,
        )
        .resolve(&root, &resolved, &self.control)?;
        self.log(format!("Java 要求来源：{}", plan.origin));
        let requirement = plan.requirement;
        let required = requirement.recommended();
        let session = self.account_session(&config)?;
        let resolution = crate::runtime_service::JavaRuntimeService::new(self).resolve(
            &config,
            id,
            requirement,
        )?;
        if let Some(warning) = resolution.warning() {
            self.log(warning);
        }
        let runtime = resolution.into_runtime();
        self.log(format!(
            "{} · {} · {}",
            runtime.description,
            runtime.architecture.rule_name(),
            runtime.path.display()
        ));
        if runtime.major() != required {
            self.log(format!(
                "未使用建议的 Java {required}，当前为 Java {}；若兼容性异常请安装建议版本",
                runtime.major()
            ));
        }
        let platform = crate::launch_platform::LaunchPlatform::new(runtime.architecture);
        let launch_meta = platform.profile(meta);
        let native_plan = crate::native_patch_service::current_service(runtime.architecture)
            .patch(
                &launch_meta,
                &resolved.chain,
                config.patch_natives,
                &self.control,
            )?;
        for message in &native_plan.messages {
            self.log(message);
        }
        let meta = &native_plan.profile;
        let launch_libraries = self.prepare_launch_libraries(
            &root,
            meta,
            &platform,
            config.download_threads,
            Some((&config, &resolved.jar_id)),
        )?;
        let native_base = version_dir.join("tui-mc-natives").join(format!(
            "{}-{}",
            std::env::consts::OS,
            runtime.architecture.rule_name()
        ));
        let mut native_directory =
            crate::native_service::NativeDirectory::create(&native_base, &self.control)?;
        let mut cp = crate::classpath_service::LaunchClasspath::default();
        for (path, native, lib) in launch_libraries {
            if native {
                native_directory.extract(&path, lib)?;
            } else {
                cp.add_library(path);
            }
        }
        let natives = native_directory.path()?;
        let separator = if cfg!(windows) { ";" } else { ":" };
        let classpath = cp.render(&jar, separator)?;
        let uuid = session.uuid.clone();
        let asset_id = meta["assetIndex"]["id"].as_str().unwrap_or(id);
        valid_id(asset_id)?;
        let index_path = root.join("assets/indexes").join(format!("{asset_id}.json"));
        if !index_path.is_file() {
            bail!("资源索引缺失，请先安装/修复此版本");
        }
        if let Some(hash) = meta["assetIndex"]["sha1"].as_str() {
            if !verify(&index_path, hash)? {
                bail!("资源索引校验失败，请先修复此版本");
            }
        }
        let index: Value = serde_json::from_slice(&fs::read(&index_path)?)?;
        let game_assets =
            crate::asset_layout::reconstruct(&root, &game_dir, asset_id, &index, &self.control)?;
        let assets = root.join("assets");
        let values: HashMap<&str, String> = [
            ("auth_player_name", session.name.clone()),
            ("auth_uuid", uuid),
            ("auth_access_token", session.access_token.to_string()),
            (
                "auth_session",
                if session.user_type == "msa" {
                    format!("token:{}:{}", session.access_token.as_str(), session.uuid)
                } else {
                    "0".into()
                },
            ),
            ("user_type", session.user_type.into()),
            ("user_properties", "{}".into()),
            ("version_name", id.into()),
            (
                "version_type",
                meta["type"].as_str().unwrap_or("release").into(),
            ),
            ("game_directory", game_dir.display().to_string()),
            ("assets_root", assets.display().to_string()),
            ("game_assets", game_assets.display().to_string()),
            ("assets_index_name", asset_id.into()),
            ("natives_directory", natives.display().to_string()),
            ("classpath", classpath.clone()),
            ("classpath_separator", separator.into()),
            ("primary_jar", jar.display().to_string()),
            (
                "primary_jar_name",
                jar.file_name()
                    .context("客户端 JAR 缺少文件名")?
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("file_separator", std::path::MAIN_SEPARATOR.to_string()),
            (
                "library_directory",
                root.join("libraries").display().to_string(),
            ),
            ("launcher_name", env!("CARGO_PKG_NAME").into()),
            ("launcher_version", env!("CARGO_PKG_VERSION").into()),
            ("auth_xuid", session.xuid.clone()),
            ("clientid", session.client_id.clone()),
            ("resolution_width", config.width.to_string()),
            ("resolution_height", config.height.to_string()),
            (
                "quickPlayMultiplayer",
                crate::launch_options::quickplay_address(&config.server)?,
            ),
            (
                "quickPlayPath",
                game_dir.join("tui-mc-quickplay.json").display().to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let mut args = vec![format!("-Xmx{}M", config.memory_mb)];
        if native_plan.lwjgl_changed {
            // This property is read only by Sodium; keep it before user arguments
            // so an explicit user choice still takes precedence.
            args.push("-Dsodium.checks.issue2561=false".into());
        }
        let platform_jvm_args = platform.arguments(&meta["arguments"]["jvm"], &HashMap::new());
        if !platform_jvm_args.is_empty() {
            args.extend(platform_jvm_args);
        } else {
            args.extend([
                "-Djava.library.path=${natives_directory}".into(),
                "-cp".into(),
                "${classpath}".into(),
            ]);
        }
        if let Some(logging) = meta.pointer("/logging/client") {
            let name = logging["file"]["id"].as_str().context("日志配置缺少 ID")?;
            let path = safe_path(&root.join("assets/log_configs"), name)?;
            if !path.is_file() {
                bail!("日志配置缺失，请先修复此版本");
            }
            if let Some(argument) = logging["argument"].as_str() {
                args.push(argument.replace("${path}", &path.display().to_string()));
            }
        }
        let parser = crate::argument_service::ArgumentParser::current();
        args.extend(parser.parse(&config.jvm_args).context("JVM 参数格式错误")?);
        apply_platform_jvm_args(&mut args, meta, std::env::consts::OS);
        let jvm_arg_count = args.len();
        args.push(meta["mainClass"].as_str().context("缺少 mainClass")?.into());
        args.extend(crate::launch_options::game_arguments_for(
            &config,
            meta,
            &platform.rules(),
        )?);
        args.extend(
            parser
                .parse(&config.game_args)
                .context("游戏参数格式错误")?,
        );
        for arg in &mut args {
            *arg = crate::launch_options::expand_argument(arg, &values)?;
        }
        crate::classpath_service::BootstrapCompatibility::from_metadata(meta)?.rewrite(
            &mut args[..jvm_arg_count],
            cp.libraries(),
            &jar,
        )?;
        self.control.check()?;
        let log_path = game_dir.join("tui-mc-game.log");
        let builtin_versions = crate::fabric_check::builtins(
            meta,
            resolved.chain.first().context("版本链为空")?,
            Some(runtime.major()),
        );
        if builtin_versions.contains_key("fabricloader") {
            crate::fabric_check::audit(
                &crate::fabric_check::active_paths(&config, id)?,
                &builtin_versions,
            )?;
        }
        self.control.check()?;
        // Hold the directory lease until exit: shared game directories must not
        // have their options changed by another launcher process during a game.
        let _game_options = crate::download_lock::DownloadLease::acquire(
            &game_dir.join("options.txt"),
            &self.control,
            || self.log("等待使用同一游戏目录的进程退出…"),
        )?;
        let language = crate::language_service::GameLanguage::parse(&config.game_language)?
            .resolve(&config.language)?;
        if language.enabled() {
            if let Some(code) =
                language.for_version(resolved.chain.first().context("版本链为空")?, meta)
            {
                let options = crate::language_service::GameOptionsService;
                if code != code.to_lowercase()
                    && !options.legacy_language_available(&jar, &index, &code)?
                {
                    self.log(format!(
                        "旧版游戏不含语言 {code}，保留原语言，避免旧版语言加载崩溃"
                    ));
                } else {
                    options.synchronize(&game_dir, &code)?;
                    self.log(format!("游戏语言：{code}"));
                }
            } else {
                self.log("此版本不支持语言设置或无法确认语言格式，保留游戏原设置");
            }
        }
        self.control.check()?;
        let launch_started = std::time::SystemTime::now();
        let output = fs::File::create(&log_path)?;
        let mut tail = crate::logs::LogTail::open(&log_path)?;
        let mut child = Command::new(&runtime.path)
            .args(args)
            .current_dir(&game_dir)
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .spawn()
            .map(GameProcess)
            .context("Java 启动失败")?;
        self.log(format!(
            "游戏进程 {} 已启动，日志：{}",
            child.id(),
            log_path.display()
        ));
        let _ = self.tx.send(Message::Progress("游戏运行中".into(), 0, 0));
        let status = match ProcessMonitor::new(&self.control).run(
            &mut child,
            &mut tail,
            |line| {
                self.log(format!(
                    "游戏 · {}",
                    redact_token(&line, &session.access_token)
                ));
            },
            |warning| self.log(redact_token(&warning, &session.access_token)),
        )? {
            ProcessOutcome::Exited(status) => status,
            ProcessOutcome::Cancelled => bail!("游戏已由用户终止"),
        };
        if !status.success() {
            match crate::diagnostics::analyze_dir(&game_dir, Some(launch_started)) {
                Ok(diagnosis) => {
                    for line in diagnosis.summary().lines() {
                        self.log(line);
                    }
                }
                Err(error) => self.log(format!("读取启动诊断失败：{error}")),
            }
            bail!("游戏异常退出 ({status})，查看 {}", log_path.display());
        }
        Ok(())
    }
}
fn redact_token(line: &str, token: &str) -> String {
    if token.len() > 8 {
        line.replace(token, "[令牌已隐藏]")
    } else {
        line.into()
    }
}
fn verify(path: &Path, expected: &str) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha1::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected))
}
fn apply_platform_jvm_args(args: &mut Vec<String>, meta: &Value, os: &str) {
    let lwjgl3 = meta["libraries"].as_array().is_some_and(|libraries| {
        libraries.iter().any(|lib| {
            lib["name"].as_str().is_some_and(|name| {
                name.starts_with("org.lwjgl:lwjgl:3.") || name.starts_with("org.lwjgl:lwjgl-glfw:")
            })
        })
    });
    if os == "macos" && lwjgl3 && !args.iter().any(|arg| arg == "-XstartOnFirstThread") {
        args.push("-XstartOnFirstThread".into());
    }
}

fn artifacts(lib: &Value) -> Result<Vec<(Value, bool)>> {
    crate::library_service::LibraryResolver::current().resolve(lib)
}
pub fn installed(config: &Config) -> Vec<String> {
    let mut versions = vec![];
    if let Ok(entries) = fs::read_dir(config.game_dir.join("versions")) {
        for e in entries.flatten() {
            let id = e.file_name().to_string_lossy().to_string();
            if e.path().join(format!("{id}.json")).is_file() {
                versions.push(id);
            }
        }
    }
    versions.sort();
    versions.reverse();
    versions
}
pub fn offline_uuid(name: &str) -> String {
    let mut bytes: [u8; 16] = Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn patched_legacy_native_reaches_launch_preflight_cache_and_extraction_without_network() {
        use crate::native_patch_service::{
            NativeArtifact, NativePatchCatalog, NativePatchService, NativeReplacement,
        };
        struct Fixture(NativeArtifact);
        impl NativePatchCatalog for Fixture {
            fn replacement(&self, _: &str, source: &str) -> Result<NativeReplacement> {
                Ok(
                    if source == "org.lwjgl.lwjgl:lwjgl-platform:2.9.1:natives" {
                        NativeReplacement::Artifact("test:native:1:arm64".into(), self.0.clone())
                    } else {
                        NativeReplacement::Keep
                    },
                )
            }
        }
        let workspace = crate::test_support::Workspace::new();
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        archive
            .start_file(
                "native-fixture.txt",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive
            .write_all(b"fixture without executing native code")
            .unwrap();
        let bytes = archive.finish().unwrap().into_inner();
        let fixture = Fixture(serde_json::from_value(serde_json::json!({
            "url":"https://fixture.invalid/native.jar", "sha1":format!("{:x}",Sha1::digest(&bytes)),
            "size":bytes.len(),"extracted":true
        })).unwrap());
        let path = workspace
            .path
            .join("libraries")
            .join(crate::version::maven_path("test:native:1:arm64").unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut backend = Backend::new(tx).unwrap();
        backend.client = Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let source = serde_json::json!({"libraries":[{"name":"org.lwjgl.lwjgl:lwjgl-platform:2.9.1",
            "natives":{os_name():"natives-old"}}]});
        let plan = NativePatchService::new(
            os_name(),
            crate::java_architecture::JavaArchitecture::Arm64,
            &fixture,
        )
        .patch(&source, &[], true, &backend.control)
        .unwrap();
        let platform = crate::launch_platform::LaunchPlatform::new(
            crate::java_architecture::JavaArchitecture::Arm64,
        );
        let prepared = backend
            .prepare_launch_libraries(&workspace.path, &plan.profile, &platform, 1, None)
            .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].0, path);
        assert!(prepared[0].1);
        let mut directory = crate::native_service::NativeDirectory::create(
            &workspace.path.join("natives"),
            &backend.control,
        )
        .unwrap();
        directory.extract(&prepared[0].0, prepared[0].2).unwrap();
        assert_eq!(
            fs::read(directory.path().unwrap().join("native-fixture.txt")).unwrap(),
            b"fixture without executing native code"
        );
        assert_eq!(
            source["libraries"][0]["name"],
            "org.lwjgl.lwjgl:lwjgl-platform:2.9.1"
        );
    }
    #[test]
    fn launch_preflight_downloads_target_jvm_libraries_and_reuses_verified_cache() {
        use std::{io::Cursor, net::TcpListener, thread, time::Instant};
        let workspace = crate::test_support::Workspace::new();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("test.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"fixture").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let hash = format!("{:x}", Sha1::digest(&bytes));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let body = bytes.clone();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = vec![];
            while requests.len() < 2 && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).unwrap();
                requests.push(
                    String::from_utf8_lossy(&buffer[..count])
                        .lines()
                        .next()
                        .unwrap()
                        .to_owned(),
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
            requests
        });
        let entry = |path: &str| serde_json::json!({"path":path,"url":format!("{base}/{path}"),"sha1":hash,"size":bytes.len()});
        let meta = serde_json::json!({"libraries":[
            {"name":"test:binding:1","downloads":{"artifact":entry("main.jar"),"classifiers":{
                "native-32":entry("native32.jar"),"native-64":entry("native64.jar")}},
                "natives":{os_name():"native-${arch}"}},
            {"name":"test:arm:1","rules":[{"action":"allow","os":{"arch":"aarch64"}}],
                "downloads":{"artifact":entry("unselected.jar")}}
        ]});
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut backend = Backend::new(tx).unwrap();
        backend.client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let platform = crate::launch_platform::LaunchPlatform::new(
            crate::java_architecture::JavaArchitecture::X86,
        );
        let prepared = backend.prepare_launch_libraries(&workspace.path, &meta, &platform, 1, None);
        let requests = server.join().unwrap();
        let prepared = prepared.unwrap();
        assert_eq!(prepared.len(), 2);
        assert!(!prepared[0].1);
        assert!(prepared[1].1);
        assert_eq!(
            requests,
            ["GET /main.jar HTTP/1.1", "GET /native32.jar HTTP/1.1"]
        );
        for (path, _, _) in &prepared {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert!(!workspace.path.join("libraries/native64.jar").exists());
        assert!(!workspace.path.join("libraries/unselected.jar").exists());
        // Listener is closed: a second pass must use validated files without a request.
        backend
            .prepare_launch_libraries(&workspace.path, &meta, &platform, 1, None)
            .unwrap();
        backend.control.cancel();
        assert!(backend
            .prepare_launch_libraries(&workspace.path, &meta, &platform, 1, None)
            .is_err());
    }
    #[test]
    fn macos_glfw_requires_first_thread_without_duplicate_or_legacy_flag() {
        let meta = serde_json::json!({"libraries":[{"name":"org.lwjgl:lwjgl-glfw:3.3.3"}]});
        let mut args = vec![];
        apply_platform_jvm_args(&mut args, &meta, "linux");
        assert!(args.is_empty());
        apply_platform_jvm_args(&mut args, &serde_json::json!({}), "macos");
        assert!(args.is_empty());
        apply_platform_jvm_args(&mut args, &meta, "macos");
        apply_platform_jvm_args(&mut args, &meta, "macos");
        assert_eq!(args, ["-XstartOnFirstThread"]);
    }
    #[test]
    fn java_offline_uuid() {
        assert_eq!(offline_uuid("Notch"), "b50ad385829d3141a2167e7d7539ba7f");
    }
    #[test]
    fn rejects_corrupt_download() {
        let path = std::env::temp_dir().join(format!("tui-mc-hash-{}", std::process::id()));
        fs::write(&path, b"abc").unwrap();
        assert!(verify(&path, "a9993e364706816aba3e25717850c26c9cd0d89d").unwrap());
        assert!(!verify(&path, "bad").unwrap());
        fs::remove_file(path).unwrap();
    }
    #[test]
    #[ignore = "requires java and javac on PATH"]
    fn real_jvm_launch_with_spaced_paths() {
        let root = std::env::temp_dir().join(format!("tui mc launch {}", std::process::id()));
        let dir = root.join("versions/fixture");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(root.join("assets/indexes")).unwrap();
        fs::write(root.join("assets/indexes/fixture.json"), b"{}").unwrap();
        let source = dir.join("LaunchFixture.java");
        fs::write(&source, "public class LaunchFixture { public static void main(String[] args) { for (String arg : args) System.out.println(arg); } }").unwrap();
        assert!(Command::new("javac")
            .args(["--release", "8"])
            .arg(&source)
            .status()
            .unwrap()
            .success());
        let mut archive = zip::ZipWriter::new(fs::File::create(dir.join("fixture.jar")).unwrap());
        archive
            .start_file(
                "LaunchFixture.class",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive
            .write_all(&fs::read(dir.join("LaunchFixture.class")).unwrap())
            .unwrap();
        archive.finish().unwrap();
        let metadata = serde_json::json!({
            "mainClass":"LaunchFixture", "libraries":[], "type":"release",
            "javaVersion":{"majorVersion":8}, "assetIndex":{"id":"fixture"},
            "arguments":{"jvm":["-cp","${classpath}"], "game":["${auth_player_name}","${auth_uuid}","${game_directory}"]}
        });
        fs::write(
            dir.join("fixture.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let config = Config {
            game_dir: root.clone(),
            username: "Notch".into(),
            memory_mb: 512,
            java: "java".into(),
            ..Config::default()
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        Backend::new(tx)
            .unwrap()
            .launch(&config, "fixture")
            .unwrap();
        let log = fs::read_to_string(dir.join("tui-mc-game.log")).unwrap();
        assert!(log.contains("Notch"));
        assert!(log.contains(&offline_uuid("Notch")));
        assert!(log.contains("tui mc launch"));
        // Remove only files created by this test; no recursive directory deletion.
        for file in [
            "LaunchFixture.java",
            "LaunchFixture.class",
            "fixture.jar",
            "fixture.json",
            "tui-mc-game.log",
        ] {
            fs::remove_file(dir.join(file)).unwrap();
        }
        fs::remove_dir(dir.join("tui-mc-natives").join(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )))
        .unwrap();
        fs::remove_dir(dir.join("tui-mc-natives")).unwrap();
        fs::remove_dir(&dir).unwrap();
        fs::remove_dir(root.join("versions")).unwrap();
        fs::remove_file(root.join("assets/indexes/fixture.json")).unwrap();
        fs::remove_dir(root.join("assets/indexes")).unwrap();
        fs::remove_dir(root.join("assets")).unwrap();
        for file in fs::read_dir(root.join("tui-mc-locks")).unwrap() {
            fs::remove_file(file.unwrap().path()).unwrap();
        }
        fs::remove_dir(root.join("tui-mc-locks")).unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    #[ignore = "requires access to official Mojang services"]
    fn official_metadata_download() {
        let (tx, rx) = std::sync::mpsc::channel();
        let backend = Backend::new(tx).unwrap();
        backend.catalog().unwrap();
        let Message::Versions(versions) = rx.recv().unwrap() else {
            panic!("missing catalog");
        };
        let version = versions.iter().find(|v| v.id == "1.21.1").unwrap();
        let workspace = crate::test_support::Workspace::new();
        let dir = workspace.path.join("metadata");
        let path = dir.join("version.json");
        backend
            .fetch(&version.url, &path, Some(&version.sha1))
            .unwrap();
        let meta: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(meta["id"], "1.21.1");
        assert!(meta["mainClass"].is_string());
        assert!(!arguments(&meta["arguments"]["jvm"]).is_empty());
        for lib in meta["libraries"].as_array().unwrap() {
            if allowed(lib) {
                for (entry, _) in artifacts(lib).unwrap() {
                    safe_path(&dir, entry["path"].as_str().unwrap()).unwrap();
                }
            }
        }
        fs::remove_file(path).unwrap();
    }
}
