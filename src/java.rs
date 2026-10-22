use crate::{
    backend::Backend, config::Config, download::Download, java_version::JavaVersion,
    metadata::valid_id,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct JavaRuntime {
    pub path: PathBuf,
    pub version: JavaVersion,
    pub architecture: crate::java_architecture::JavaArchitecture,
    pub description: String,
}
impl JavaRuntime {
    pub fn major(&self) -> u64 {
        self.version.major()
    }
}
pub fn discover(config: &Config, control: &crate::task::TaskControl) -> Result<Vec<JavaRuntime>> {
    control.check()?;
    let mut paths = crate::runtime_repository::installed_paths(config, control)?;
    paths.push(PathBuf::from("java"));
    for name in ["JAVA_HOME", "JDK_HOME"] {
        if let Some(home) = std::env::var_os(name) {
            paths.push(PathBuf::from(home).join("bin").join(executable()));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(executable());
            if candidate.is_file() {
                paths.push(candidate);
            }
        }
    }
    let mut roots = vec![config.game_dir.join("runtime")];
    if cfg!(windows) {
        for env in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(env) {
                let root = PathBuf::from(root);
                for name in [
                    "Java",
                    "Microsoft",
                    "Eclipse Adoptium",
                    "BellSoft",
                    "Zulu",
                    "Amazon Corretto",
                    "Programs/Eclipse Adoptium",
                ] {
                    roots.push(root.join(name));
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        roots.extend([
            PathBuf::from("/Library/Java/JavaVirtualMachines"),
            PathBuf::from("/opt/homebrew/opt"),
            PathBuf::from("/usr/local/opt"),
        ]);
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Library/Java/JavaVirtualMachines"));
        }
    } else {
        roots.extend([
            PathBuf::from("/usr/lib/jvm"),
            PathBuf::from("/usr/java"),
            PathBuf::from("/opt/java"),
        ]);
    }
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".sdkman/candidates/java"));
        roots.push(PathBuf::from(home).join(".jdks"));
    }
    let mut visited = HashSet::new();
    for root in roots {
        control.check()?;
        scan(&root, 0, &mut 800, &mut visited, &mut paths);
    }
    let mut seen = HashSet::new();
    let mut found = vec![];
    let probe = crate::java_probe::JavaProbe::new(control);
    for path in paths {
        control.check()?;
        let canonical = crate::paths::for_java(&fs::canonicalize(&path).unwrap_or(path));
        let key = if cfg!(windows) {
            canonical.to_string_lossy().to_lowercase()
        } else {
            canonical.to_string_lossy().into_owned()
        };
        if !seen.insert(key) {
            continue;
        }
        if let Ok((version, architecture, description)) = probe.inspect(&canonical) {
            found.push(JavaRuntime {
                path: canonical,
                version,
                architecture,
                description,
            });
        }
    }
    control.check()?;
    found.sort_by_key(JavaRuntime::major);
    Ok(found)
}
fn scan(
    root: &Path,
    depth: usize,
    budget: &mut usize,
    visited: &mut HashSet<PathBuf>,
    paths: &mut Vec<PathBuf>,
) {
    if root.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .eq_ignore_ascii_case(crate::runtime_repository::MANAGED_DIR)
    }) {
        return;
    }
    if root.join(".installing").exists() {
        return;
    }
    if depth > 5 || *budget == 0 {
        return;
    }
    *budget -= 1;
    let Ok(canonical) = fs::canonicalize(root) else {
        return;
    };
    if !visited.insert(canonical) {
        return;
    }
    let direct = root.join("bin").join(executable());
    if direct.is_file() {
        paths.push(direct);
        return;
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                scan(&entry.path(), depth + 1, budget, visited, paths);
            }
        }
    }
}
fn executable() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}
pub fn select(
    config: &Config,
    required: u64,
    control: &crate::task::TaskControl,
) -> Result<JavaRuntime> {
    if config.java != "auto" {
        let (version, architecture, description) =
            crate::java_probe::JavaProbe::new(control).inspect(Path::new(&config.java))?;
        let major = version.major();
        if major < required {
            bail!("需要 Java {required}，当前 Java {major}；可将 Java 路径设为 auto 自动匹配");
        }
        return Ok(JavaRuntime {
            path: crate::paths::for_java(
                &fs::canonicalize(&config.java).unwrap_or_else(|_| PathBuf::from(&config.java)),
            ),
            version,
            architecture,
            description,
        });
    }
    choose(discover(config, control)?, required)
}
pub(crate) fn choose(runtimes: Vec<JavaRuntime>, required: u64) -> Result<JavaRuntime> {
    runtimes
        .into_iter()
        .filter(|j| j.major() >= required)
        .min_by(|first, second| {
            first
                .major()
                .cmp(&second.major())
                .then_with(|| first.version.is_preview().cmp(&second.version.is_preview()))
                .then_with(|| {
                    second
                        .architecture
                        .is_launcher_arch()
                        .cmp(&first.architecture.is_launcher_arch())
                })
                .then_with(|| second.version.compare(&first.version))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "未找到兼容的 Java {required}；请安装后重新检测，或指定 Java 可执行文件"
            )
        })
}
pub const RUNTIME_MANIFEST: &str = "https://piston-meta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
pub fn component_for(major: u64) -> Result<&'static str> {
    match major {
        8 => Ok("jre-legacy"),
        16 => Ok("java-runtime-alpha"),
        17 => Ok("java-runtime-gamma"),
        21 => Ok("java-runtime-delta"),
        25 => Ok("java-runtime-epsilon"),
        _ => bail!("没有已知的 Mojang Java {major} 组件，请手动指定 Java"),
    }
}
fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("windows-x64"),
        ("windows", "x86") => Ok("windows-x86"),
        ("windows", "aarch64") => Ok("windows-arm64"),
        ("linux", "x86_64") => Ok("linux"),
        ("linux", "x86") => Ok("linux-i386"),
        ("macos", "x86_64") => Ok("mac-os"),
        ("macos", "aarch64") => Ok("mac-os-arm64"),
        _ => bail!(
            "Mojang 没有此平台的 Java 运行时；请安装与系统架构一致的 Java，并在设置中指定路径"
        ),
    }
}
impl Backend {
    pub fn install_java(
        &self,
        config: &Config,
        component: &str,
        required: u64,
    ) -> Result<JavaRuntime> {
        valid_id(component)?;
        self.control.check()?;
        let platform = platform()?;
        let catalog = self.cached_metadata(RUNTIME_MANIFEST, parse_runtime_catalog)?;
        let candidate = catalog[platform][component]
            .as_array()
            .and_then(|a| a.first())
            .context("官方目录中没有适用的 Java 组件")?;
        let version = candidate["version"]["name"]
            .as_str()
            .context("Java 组件没有版本名")?;
        self.log(format!("安装 Mojang Java {version} · {component}"));
        let root = config
            .root()?
            .join("runtime")
            .join(crate::runtime_repository::MANAGED_DIR)
            .join(component)
            .join(platform);
        let descriptor = &candidate["manifest"];
        let fingerprint = descriptor["sha1"].as_str().context("Java 清单缺少 SHA-1")?;
        let session = crate::runtime_repository::RuntimeGeneration::begin(
            &root,
            fingerprint,
            &self.control,
            || {
                self.log("等待另一个任务完成同一 Java 组件安装…");
            },
        )?;
        let root = session.root();
        let manifest_path = root.join(".manifest.json");
        self.downloader().fetch(&Download {
            url: descriptor["url"]
                .as_str()
                .context("Java 清单没有 URL")?
                .into(),
            dest: manifest_path.clone(),
            sha1: Some(fingerprint.into()),
            size: descriptor["size"].as_u64(),
            alternatives: vec![],
        })?;
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
        let files = manifest["files"].as_object().context("Java 文件清单无效")?;
        match session.reusable(files) {
            Ok(Some(existing)) => {
                let path = crate::runtime_repository::executable(&existing);
                match crate::java_probe::JavaProbe::new(&self.control).inspect(&path) {
                    Ok((version, architecture, description))
                        if version.major() == required && architecture.is_launcher_arch() =>
                    {
                        self.log("已验证并复用现有 Java 运行时");
                        return Ok(JavaRuntime {
                            path,
                            version,
                            architecture,
                            description,
                        });
                    }
                    Ok(_) => self.log("现有 Java 主版本或架构不符，将准备独立运行时"),
                    Err(error) => {
                        self.control.check()?;
                        self.log(format!("现有 Java 无法复用，将准备新目录：{error:#}"));
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.control.check()?;
                self.log(format!("已发布 Java 校验失败，将准备独立运行时：{error:#}"));
            }
        }
        let mut downloads = vec![];
        for (name, file) in files {
            let dest = session.destination(name)?;
            match file["type"].as_str() {
                Some("directory") => fs::create_dir_all(dest)?,
                Some("file") => {
                    let raw = &file["downloads"]["raw"];
                    downloads.push(Download {
                        url: raw["url"].as_str().context("Java 文件缺少 raw URL")?.into(),
                        dest,
                        sha1: raw["sha1"].as_str().map(str::to_owned),
                        size: raw["size"].as_u64(),
                        alternatives: vec![],
                    });
                }
                Some("link") => {}
                _ => bail!("未知 Java 文件类型"),
            }
        }
        self.downloader()
            .many(downloads, config.download_threads, "Java 运行时")?;
        for (name, file) in files {
            self.control.check()?;
            let dest = session.destination(name)?;
            if file["type"] == "link" {
                let target = file["target"].as_str().context("Java 链接没有目标")?;
                let source = checked_link_target(root, &dest, target)?;
                if !dest.exists() {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(target, &dest)?;
                    #[cfg(windows)]
                    {
                        if source.is_dir() {
                            bail!("此 Java 组件需要目录符号链接，请手动安装");
                        }
                        fs::copy(&source, &dest)?;
                    }
                }
                let _ = source;
            }
            #[cfg(unix)]
            if file["executable"] == true {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
            }
        }
        let path = if root.join("bin").join(executable()).is_file() {
            root.join("bin").join(executable())
        } else {
            root.join("jre.bundle/Contents/Home/bin").join(executable())
        };
        let (version, architecture, description) =
            crate::java_probe::JavaProbe::new(&self.control).inspect(&path)?;
        let major = version.major();
        if major != required {
            bail!("下载后的 Java 版本不符：预期 {required}，实际 {major}");
        }
        if !architecture.is_launcher_arch() {
            bail!(
                "下载后的 Java 架构不符：预期 {}，实际 {}",
                std::env::consts::ARCH,
                architecture.rule_name()
            );
        }
        let published = session.publish()?;
        let path = crate::runtime_repository::executable(&published);
        Ok(JavaRuntime {
            path,
            version,
            architecture,
            description,
        })
    }
    pub fn install_java_for_version(&self, config: &Config, id: &str) -> Result<JavaRuntime> {
        let resolved = crate::version::resolve(&config.root()?, id)?;
        let plan = crate::game_java_service::GameJavaResolver::new(
            &crate::game_java_service::JarClientVersionSource,
        )
        .resolve(&config.root()?, &resolved, &self.control)?;
        let requirement = plan.requirement;
        let major = requirement.recommended();
        let component = match plan.component {
            Some(component) => component,
            None => component_for(major)?.into(),
        };
        requirement.check(self.install_java(config, &component, major)?)
    }
}
fn parse_runtime_catalog(bytes: &[u8]) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let platforms = value
        .as_object()
        .filter(|platforms| !platforms.is_empty())
        .context("Java 组件目录格式无效")?;
    for platform in platforms.values() {
        for component in platform
            .as_object()
            .context("Java 平台目录格式无效")?
            .values()
        {
            for entry in component.as_array().context("Java 组件版本列表格式无效")? {
                entry["version"]["name"]
                    .as_str()
                    .filter(|name| !name.trim().is_empty())
                    .context("Java 组件没有版本名")?;
                let descriptor = &entry["manifest"];
                let hash = descriptor["sha1"].as_str().context("Java 清单缺少 SHA-1")?;
                if hash.len() != 40 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                    bail!("Java 清单 SHA-1 无效");
                }
                crate::metadata_cache::validate_url(
                    descriptor["url"].as_str().context("Java 清单缺少 URL")?,
                )
                .context("Java 清单 URL 无效")?;
            }
        }
    }
    Ok(value)
}
pub(crate) fn checked_link_target(root: &Path, dest: &Path, target: &str) -> Result<PathBuf> {
    use std::path::Component;
    let base = dest.parent().context("Java 链接没有父目录")?;
    let mut resolved = base.to_path_buf();
    for component in Path::new(target).components() {
        match component {
            Component::Normal(c) => resolved.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() || !resolved.starts_with(root) {
                    bail!("Java 链接越过运行时根目录");
                }
            }
            _ => bail!("Java 链接必须使用相对路径"),
        }
    }
    if !resolved.starts_with(root) {
        bail!("Java 链接不在运行时根目录");
    }
    Ok(resolved)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_catalog_cache_validates_descriptors_without_requiring_all_components() {
        let catalog = serde_json::json!({"windows-x64":{"jre-legacy":[{"version":{"name":"1.8.0_51"},"manifest":{"url":"https://example.invalid/manifest","sha1":"a".repeat(40)}}],"java-runtime-delta":[]}});
        assert!(parse_runtime_catalog(&serde_json::to_vec(&catalog).unwrap()).is_ok());
        let mut broken = catalog.clone();
        broken["windows-x64"]["jre-legacy"][0]["manifest"]["sha1"] = "bad".into();
        assert!(parse_runtime_catalog(&serde_json::to_vec(&broken).unwrap()).is_err());
        let mut broken = catalog;
        broken["windows-x64"]["jre-legacy"][0]["manifest"]["url"] =
            "http://example.invalid/manifest".into();
        assert!(parse_runtime_catalog(&serde_json::to_vec(&broken).unwrap()).is_err());
        assert!(parse_runtime_catalog(b"[]").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn discovers_linked_jdk_without_following_cycles() {
        let workspace = crate::test_support::Workspace::new();
        let root = &workspace.path;
        let bundle = root.join("jdk/Contents/Home/bin");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("java"), b"fixture").unwrap();
        fs::create_dir(root.join("opt")).unwrap();
        std::os::unix::fs::symlink(root.join("jdk"), root.join("opt/openjdk")).unwrap();
        std::os::unix::fs::symlink(root.join("opt"), root.join("opt/cycle")).unwrap();
        let mut found = Vec::new();
        scan(
            &root.join("opt"),
            0,
            &mut 800,
            &mut HashSet::new(),
            &mut found,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(
            fs::canonicalize(&found[0]).unwrap(),
            fs::canonicalize(bundle.join("java")).unwrap()
        );
    }
    #[test]
    fn selects_closest_compatible_runtime() {
        let runtimes = [8, 26, 21, 17]
            .into_iter()
            .map(|major| JavaRuntime {
                version: JavaVersion::parse(&major.to_string()).unwrap(),
                path: format!("java{major}").into(),
                description: String::new(),
                architecture: crate::java_architecture::JavaArchitecture::X64,
            })
            .collect();
        assert_eq!(choose(runtimes, 21).unwrap().major(), 21);
        assert!(choose(vec![], 21).is_err());
    }
    #[test]
    fn prefers_native_architecture_within_the_same_major_but_allows_emulated_java() {
        let native =
            crate::java_architecture::JavaArchitecture::parse(std::env::consts::ARCH).unwrap();
        let other = if native == crate::java_architecture::JavaArchitecture::Arm64 {
            crate::java_architecture::JavaArchitecture::X64
        } else {
            crate::java_architecture::JavaArchitecture::Arm64
        };
        let runtime = |arch, version| JavaRuntime {
            path: "test-java".into(),
            version: JavaVersion::parse(version).unwrap(),
            architecture: arch,
            description: String::new(),
        };
        assert_eq!(
            choose(
                vec![runtime(other, "21.0.8"), runtime(native, "21.0.7")],
                21
            )
            .unwrap()
            .architecture,
            native
        );
        assert_eq!(
            choose(vec![runtime(other, "21"), runtime(native, "25")], 21)
                .unwrap()
                .architecture,
            other
        );
        assert_eq!(
            choose(vec![runtime(other, "21")], 21).unwrap().architecture,
            other
        );
    }
    #[test]
    fn chooses_newest_stable_update_within_the_closest_major_version() {
        for (versions, required, expected) in [
            (vec!["1.8.0_51", "1.8.0_451", "17.0.12"], 8, "1.8.0_451"),
            (vec!["17.0.9", "17.0.10", "21.0.7"], 17, "17.0.10"),
            (vec!["21.0.8-ea+7", "21.0.7+6", "25"], 21, "21.0.7+6"),
            (vec!["17.0.12+7", "17.0.12+8"], 17, "17.0.12+8"),
        ] {
            let runtimes = versions
                .into_iter()
                .map(|text| JavaRuntime {
                    path: format!("java-{text}").into(),
                    version: JavaVersion::parse(text).unwrap(),
                    description: String::new(),
                    architecture: crate::java_architecture::JavaArchitecture::X64,
                })
                .collect();
            assert_eq!(
                choose(runtimes, required).unwrap().version.to_string(),
                expected
            );
        }
    }
    #[test]
    fn generic_scan_keeps_legacy_java_but_does_not_enter_managed_generations() {
        let workspace = crate::test_support::Workspace::new();
        let root = workspace.path.join("runtime");
        let legacy = root.join("legacy/bin");
        let private = root
            .join(crate::runtime_repository::MANAGED_DIR)
            .join("component/platform/.pending-hash/bin");
        for dir in [&legacy, &private] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join(executable()), b"fixture, never executed").unwrap();
        }
        let mut paths = vec![];
        scan(&root, 0, &mut 800, &mut HashSet::new(), &mut paths);
        assert_eq!(paths, [legacy.join(executable())]);
    }
}
