use crate::{
    backend::{java_info, Backend},
    config::Config,
    download::Download,
    metadata::{safe_path, valid_id},
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
    pub major: u64,
    pub description: String,
}
pub fn discover(config: &Config) -> Vec<JavaRuntime> {
    let mut paths = vec![PathBuf::from("java")];
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
        scan(&root, 0, &mut 800, &mut visited, &mut paths);
    }
    let mut seen = HashSet::new();
    let mut found = vec![];
    for path in paths {
        let canonical = crate::paths::for_java(&fs::canonicalize(&path).unwrap_or(path));
        let key = if cfg!(windows) {
            canonical.to_string_lossy().to_lowercase()
        } else {
            canonical.to_string_lossy().into_owned()
        };
        if !seen.insert(key) {
            continue;
        }
        if let Ok((major, description)) = java_info(&canonical.to_string_lossy()) {
            found.push(JavaRuntime {
                path: canonical,
                major,
                description,
            });
        }
    }
    found.sort_by_key(|runtime| runtime.major);
    found
}
fn scan(
    root: &Path,
    depth: usize,
    budget: &mut usize,
    visited: &mut HashSet<PathBuf>,
    paths: &mut Vec<PathBuf>,
) {
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
pub fn select(config: &Config, required: u64) -> Result<JavaRuntime> {
    if config.java != "auto" {
        let (major, description) = java_info(&config.java)?;
        if major < required {
            bail!("需要 Java {required}，当前 Java {major}；可将 Java 路径设为 auto 自动匹配");
        }
        return Ok(JavaRuntime {
            path: crate::paths::for_java(
                &fs::canonicalize(&config.java).unwrap_or_else(|_| PathBuf::from(&config.java)),
            ),
            major,
            description,
        });
    }
    choose(discover(config), required)
}
fn choose(runtimes: Vec<JavaRuntime>, required: u64) -> Result<JavaRuntime> {
    runtimes
        .into_iter()
        .filter(|j| j.major >= required)
        .min_by_key(|j| j.major)
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
        let catalog = self.get_json(RUNTIME_MANIFEST)?;
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
            .join(component)
            .join(platform);
        fs::create_dir_all(&root)?;
        fs::write(
            root.join(".installing"),
            b"Java runtime installation in progress",
        )?;
        let descriptor = &candidate["manifest"];
        let manifest_path = root.join(".manifest.json");
        self.downloader().fetch(&Download {
            url: descriptor["url"]
                .as_str()
                .context("Java 清单没有 URL")?
                .into(),
            dest: manifest_path.clone(),
            sha1: descriptor["sha1"].as_str().map(str::to_owned),
            size: descriptor["size"].as_u64(),
            alternatives: vec![],
        })?;
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(manifest_path)?)?;
        let files = manifest["files"].as_object().context("Java 文件清单无效")?;
        let mut downloads = vec![];
        for (name, file) in files {
            let dest = safe_path(&root, name)?;
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
            let dest = safe_path(&root, name)?;
            if file["type"] == "link" {
                let target = file["target"].as_str().context("Java 链接没有目标")?;
                let source = checked_link_target(&root, &dest, target)?;
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
        let (major, description) = java_info(&path.to_string_lossy())?;
        if major != required {
            bail!("下载后的 Java 版本不符：预期 {required}，实际 {major}");
        }
        fs::write(
            root.join(".complete"),
            descriptor["sha1"].as_str().unwrap_or("ok"),
        )?;
        fs::remove_file(root.join(".installing"))?;
        Ok(JavaRuntime {
            path,
            major,
            description,
        })
    }
    pub fn install_java_for_version(&self, config: &Config, id: &str) -> Result<JavaRuntime> {
        let resolved = crate::version::resolve(&config.root()?, id)?;
        let major = resolved.meta["javaVersion"]["majorVersion"]
            .as_u64()
            .unwrap_or(8);
        let component = match resolved.meta["javaVersion"]["component"].as_str() {
            Some(component) => component.to_owned(),
            None => component_for(major)?.into(),
        };
        self.install_java(config, &component, major)
    }
}
fn checked_link_target(root: &Path, dest: &Path, target: &str) -> Result<PathBuf> {
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
                major,
                path: format!("java{major}").into(),
                description: String::new(),
            })
            .collect();
        assert_eq!(choose(runtimes, 21).unwrap().major, 21);
        assert!(choose(vec![], 21).is_err());
    }
}
