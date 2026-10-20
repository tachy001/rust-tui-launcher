use crate::{
    config::Config,
    download::{Download, Downloader},
    metadata::*,
    task::TaskControl,
    version,
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
    thread,
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
        let dir = root.join("versions").join(&remote.id);
        self.log(format!("读取版本 {}", remote.id));
        let json_path = dir.join(format!("{}.json", remote.id));
        self.fetch(&remote.url, &json_path, Some(&remote.sha1))?;
        self.repair(config, &remote.id)
    }
    pub fn repair(&self, config: &Config, id: &str) -> Result<()> {
        self.control.check()?;
        let config = config.effective(id);
        config.validate()?;
        let root = config.root()?;
        let resolved = version::resolve(&root, id)?;
        let meta = &resolved.meta;
        self.log("校验客户端与依赖库");
        self.download_entry(
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
        self.repair_pack_files(&config, id)?;
        self.repair_mod_inventory(&config, id)?;
        self.log("安装完成，可选择此版本启动");
        Ok(())
    }
    pub fn launch(&self, config: &Config, id: &str) -> Result<()> {
        let config = config.effective(id);
        config.validate()?;
        valid_id(id)?;
        let root = config.root()?;
        let version_dir = root.join("versions").join(id);
        let resolved = version::resolve(&root, id)?;
        let meta = &resolved.meta;
        self.log(format!("版本链：{}", resolved.chain.join(" → ")));
        let _running = resolved
            .chain
            .iter()
            .map(|ancestor| {
                let dir = root.join("versions").join(ancestor);
                if ancestor == id {
                    crate::instances::InstanceLock::acquire(&dir)
                } else {
                    crate::instances::InstanceLock::shared(&dir)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let game_dir = config.instance_dir(id)?;
        let session = self.account_session(&config)?;
        let required = meta["javaVersion"]["majorVersion"].as_u64().unwrap_or(8);
        let selected = crate::java::select(&config, required);
        let runtime = if config.java == "auto"
            && selected
                .as_ref()
                .map(|j| j.major != required)
                .unwrap_or(true)
        {
            match self.install_java_for_version(&config, id) {
                Ok(runtime) => runtime,
                Err(error) => match selected {
                    Ok(runtime) => {
                        self.log(format!(
                            "安装建议 Java 失败，将尝试本机 Java {}：{error:#}",
                            runtime.major
                        ));
                        runtime
                    }
                    Err(_) => return Err(error),
                },
            }
        } else {
            selected?
        };
        self.log(format!(
            "{} · {}",
            runtime.description,
            runtime.path.display()
        ));
        if runtime.major != required {
            self.log(format!(
                "未使用建议的 Java {required}，当前为 Java {}；若兼容性异常请安装建议版本",
                runtime.major
            ));
        }
        let natives = version_dir.join("tui-mc-natives").join(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        fs::create_dir_all(&natives)?;
        let mut cp = vec![];
        for lib in meta["libraries"].as_array().context("缺少 libraries")? {
            if !allowed(lib) {
                continue;
            }
            for (entry, native) in artifacts(lib)? {
                let path = safe_path(
                    &root.join("libraries"),
                    entry["path"].as_str().context("缺少库路径")?,
                )?;
                if !path.is_file() {
                    bail!("依赖缺失，请先安装/修复：{}", path.display());
                }
                if let Some(hash) = entry["sha1"].as_str() {
                    if !verify(&path, hash)? {
                        bail!("依赖校验失败，请重新安装：{}", path.display());
                    }
                }
                if native {
                    extract_natives(&path, &natives, lib)?;
                } else {
                    cp.push(path);
                }
            }
        }
        let jar = root
            .join("versions")
            .join(&resolved.jar_id)
            .join(format!("{}.jar", resolved.jar_id));
        if !jar.exists() {
            bail!("客户端缺失，请先安装");
        }
        if let Some(hash) = meta["downloads"]["client"]["sha1"].as_str() {
            if !verify(&jar, hash)? {
                bail!("客户端校验失败，请重新安装");
            }
        }
        cp.push(jar);
        let separator = if cfg!(windows) { ";" } else { ":" };
        let classpath = cp
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join(separator);
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
        if meta["arguments"]["jvm"].is_array() && !arguments(&meta["arguments"]["jvm"]).is_empty() {
            args.extend(arguments(&meta["arguments"]["jvm"]));
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
        args.extend(shlex::split(&config.jvm_args).context("JVM 参数引号不匹配")?);
        apply_platform_jvm_args(&mut args, meta, std::env::consts::OS);
        args.push(meta["mainClass"].as_str().context("缺少 mainClass")?.into());
        args.extend(crate::launch_options::game_arguments(&config, meta)?);
        args.extend(shlex::split(&config.game_args).context("游戏参数引号不匹配")?);
        for arg in &mut args {
            for (key, value) in &values {
                *arg = arg.replace(&format!("${{{key}}}"), value);
            }
            if arg.contains("${") {
                bail!("启动参数中存在未识别的占位符，请检查版本元数据或自定义参数");
            }
        }
        self.control.check()?;
        let log_path = game_dir.join("tui-mc-game.log");
        let builtin_versions = crate::fabric_check::builtins(
            meta,
            resolved.chain.first().context("版本链为空")?,
            Some(runtime.major),
        );
        if builtin_versions.contains_key("fabricloader") {
            crate::fabric_check::audit(
                &crate::fabric_check::active_paths(&config, id)?,
                &builtin_versions,
            )?;
        }
        let launch_started = std::time::SystemTime::now();
        let output = fs::File::create(&log_path)?;
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
        let mut tail = crate::logs::LogTail::open(&log_path)?;
        let status = loop {
            for line in tail.read_lines()? {
                self.log(format!(
                    "游戏 · {}",
                    redact_token(&line, &session.access_token)
                ));
            }
            if self.control.cancelled() {
                child.kill().context("终止游戏失败")?;
                child.wait()?;
                bail!("游戏已由用户终止");
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(100));
        };
        for line in tail.read_lines()? {
            self.log(format!(
                "游戏 · {}",
                redact_token(&line, &session.access_token)
            ));
        }
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
pub(crate) struct GameProcess(pub(crate) std::process::Child);
impl std::ops::Deref for GameProcess {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for GameProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for GameProcess {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
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
    let mut result = vec![];
    if let Some(a) = lib.pointer("/downloads/artifact") {
        result.push((a.clone(), false));
    } else if lib.get("downloads").is_none() {
        let name = lib["name"].as_str().context("库缺少 Maven 坐标")?;
        let path = version::maven_path(name)?;
        let base = lib["url"]
            .as_str()
            .unwrap_or("https://libraries.minecraft.net/");
        result.push((serde_json::json!({"path":path, "url":format!("{}/{}", base.trim_end_matches('/'), path), "sha1":lib["sha1"], "size":lib["size"]}), false));
    }
    if let Some(native) = lib["natives"][os_name()].as_str() {
        let key = native.replace(
            "${arch}",
            if cfg!(target_pointer_width = "64") {
                "64"
            } else {
                "32"
            },
        );
        let entry = lib["downloads"]["classifiers"]
            .get(&key)
            .context("当前平台缺少 native 分类")?;
        result.push((entry.clone(), true));
    }
    Ok(result)
}
fn extract_natives(path: &Path, dir: &Path, lib: &Value) -> Result<()> {
    let mut archive = zip::ZipArchive::new(fs::File::open(path)?)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_owned();
        if entry.is_dir()
            || name.starts_with("META-INF/")
            || lib["extract"]["exclude"].as_array().is_some_and(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .any(|prefix| name.starts_with(prefix))
            })
        {
            continue;
        }
        let dest = safe_path(dir, &name)?;
        fs::create_dir_all(dest.parent().context("缺少 native 目录")?)?;
        std::io::copy(&mut entry, &mut fs::File::create(dest)?)?;
    }
    Ok(())
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
pub fn java_info(java: &str) -> Result<(u64, String)> {
    let output = Command::new(java)
        .arg("-version")
        .output()
        .context("找不到 Java，请在设置中指定 java 可执行文件")?;
    let info = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    if !output.status.success() {
        bail!("Java 检测失败：{info}");
    }
    let re = regex::Regex::new(r#"(?:version\s+\"|openjdk\s+)(\d+)(?:\.(\d+))?"#)?;
    let captures = re.captures(&info).context("无法识别 Java 版本")?;
    let first: u64 = captures[1].parse()?;
    let major = if first == 1 {
        captures
            .get(2)
            .context("Java 版本不完整")?
            .as_str()
            .parse()?
    } else {
        first
    };
    Ok((major, info.lines().next().unwrap_or("Java").to_owned()))
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
        let dir = std::env::temp_dir().join(format!("tui-mc-metadata-{}", std::process::id()));
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
        fs::remove_dir(dir).unwrap();
    }
}
