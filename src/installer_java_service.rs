use crate::{backend::Backend, config::Config, java::JavaRuntime, task::TaskControl};
use anyhow::{bail, Context, Result};
use std::{fs, io::Read, path::Path};

const MAX_MANIFEST_MAIN: u64 = 64 * 1024;
pub(crate) struct RunnableJarInfo {
    pub(crate) main_class: String,
    pub(crate) minimum: u64,
}
pub(crate) trait RunnableJarSource {
    fn inspect(&self, jar: &Path, control: &TaskControl) -> Result<RunnableJarInfo>;
}
pub(crate) struct JarEntryPointSource;
impl JarEntryPointSource {
    fn main_class(bytes: &[u8]) -> Result<String> {
        let mut attributes: Vec<(Vec<u8>, Vec<u8>)> = vec![];
        let mut offset = 0;
        while offset < bytes.len() {
            let end = bytes[offset..]
                .iter()
                .position(|byte| matches!(byte, b'\r' | b'\n'))
                .map_or(bytes.len(), |end| offset + end);
            let mut next = end;
            if next < bytes.len() {
                next += 1;
                if bytes[end] == b'\r' && bytes.get(next) == Some(&b'\n') {
                    next += 1;
                }
            }
            if next as u64 > MAX_MANIFEST_MAIN {
                bail!("安装器清单主段超过 64 KiB");
            }
            let line = &bytes[offset..end];
            offset = next;
            if line.is_empty() {
                break;
            }
            if let Some(continued) = line.strip_prefix(b" ") {
                attributes
                    .last_mut()
                    .context("清单续行缺少前一属性")?
                    .1
                    .extend_from_slice(continued);
            } else {
                let colon = line
                    .iter()
                    .position(|byte| *byte == b':')
                    .context("安装器清单属性缺少冒号")?;
                let name = &line[..colon];
                let value = line[colon + 1..]
                    .strip_prefix(b" ")
                    .context("清单属性冒号后缺少空格")?;
                if name.is_empty()
                    || !name
                        .iter()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                {
                    bail!("安装器清单属性名无效");
                }
                attributes.push((name.to_vec(), value.to_vec()));
            }
        }
        let mut mains = attributes
            .into_iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(b"Main-Class"));
        let (_, value) = mains.next().context("安装器清单缺少 Main-Class")?;
        if mains.next().is_some() {
            bail!("安装器清单有重复 Main-Class");
        }
        let main = String::from_utf8(value).context("安装器 Main-Class 不是 UTF-8")?;
        let main = main.trim();
        if main.is_empty()
            || main.split('.').any(str::is_empty)
            || main
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || "/\\;[:".contains(c))
        {
            bail!("安装器 Main-Class 格式无效");
        }
        Ok(main.into())
    }
    fn unique(archive: &zip::ZipArchive<fs::File>, name: &str) -> Result<()> {
        if archive.file_names().filter(|entry| *entry == name).count() != 1 {
            bail!("安装器条目缺失或重复：{name}");
        }
        Ok(())
    }
}
impl RunnableJarSource for JarEntryPointSource {
    fn inspect(&self, jar: &Path, control: &TaskControl) -> Result<RunnableJarInfo> {
        control.check()?;
        let mut archive =
            crate::installer_archive::open(jar, control).context("读取安装器 JAR 失败")?;
        Self::unique(&archive, "META-INF/MANIFEST.MF")?;
        let mut manifest = vec![];
        archive
            .by_name("META-INF/MANIFEST.MF")?
            .take(MAX_MANIFEST_MAIN + 1)
            .read_to_end(&mut manifest)?;
        let main_class = Self::main_class(&manifest)?;
        read_class_info(&mut archive, main_class, control)
    }
}
pub(crate) struct ClassPathEntrySource<'a>(pub(crate) &'a str);
impl RunnableJarSource for ClassPathEntrySource<'_> {
    fn inspect(&self, jar: &Path, control: &TaskControl) -> Result<RunnableJarInfo> {
        control.check()?;
        if self.0.is_empty()
            || self.0.split('.').any(str::is_empty)
            || self
                .0
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || "/\\;[:".contains(c))
        {
            bail!("指定的 Java 类名无效");
        }
        let mut archive = crate::installer_archive::open(jar, control)?;
        read_class_info(&mut archive, self.0.into(), control)
    }
}
fn read_class_info(
    archive: &mut zip::ZipArchive<fs::File>,
    main_class: String,
    control: &TaskControl,
) -> Result<RunnableJarInfo> {
    control.check()?;
    let entry = format!("{}.class", main_class.replace('.', "/"));
    JarEntryPointSource::unique(archive, &entry)?;
    let mut header = [0; 8];
    archive
        .by_name(&entry)?
        .read_exact(&mut header)
        .context("安装器入口 class 头不完整")?;
    if header[..4] != [0xca, 0xfe, 0xba, 0xbe] {
        bail!("安装器入口不是 Java class 文件");
    }
    let major = u16::from_be_bytes([header[6], header[7]]);
    let minor = u16::from_be_bytes([header[4], header[5]]);
    if major < 45 || (major >= 56 && minor != 0 && minor != u16::MAX) {
        bail!("安装器入口字节码版本无效");
    }
    if major >= 56 && minor == u16::MAX {
        bail!("安装器入口依赖 Java 预览特性，当前安装流程未启用预览运行参数");
    }
    control.check()?;
    Ok(RunnableJarInfo {
        main_class,
        minimum: u64::from(major - 44),
    })
}

pub(crate) struct InstallerJavaRequirement {
    pub(crate) main_class: String,
    pub(crate) installer_minimum: u64,
    pub(crate) game_minimum: u64,
    minimum: u64,
}
impl InstallerJavaRequirement {
    pub(crate) fn inspect(
        source: &dyn RunnableJarSource,
        jar: &Path,
        game_minimum: u64,
        control: &TaskControl,
    ) -> Result<Self> {
        control.check()?;
        let info = source.inspect(jar, control)?;
        control.check()?;
        if info.minimum == 0 || game_minimum == 0 {
            bail!("安装器与游戏 Java 下限必须为正整数");
        }
        Ok(Self {
            minimum: game_minimum.max(info.minimum).max(8),
            main_class: info.main_class,
            installer_minimum: info.minimum,
            game_minimum,
        })
    }
    fn check(&self, runtime: JavaRuntime) -> Result<JavaRuntime> {
        if runtime.major() < self.minimum {
            bail!(
                "安装至少需要 Java {}（安装器入口至少 {}，游戏至少 {}），当前 Java {}",
                self.minimum,
                self.installer_minimum,
                self.game_minimum,
                runtime.major()
            );
        }
        Ok(runtime)
    }
    fn provision_major(&self) -> Result<u64> {
        // Preserve the known Java 16 component for 1.17, otherwise choose the
        // first known LTS component that can satisfy this floor.
        if self.minimum == 16 {
            return Ok(16);
        }
        [8, 17, 21, 25]
            .into_iter()
            .find(|major| *major >= self.minimum)
            .context("没有满足安装器要求的已知 Java 下载组件，请手动指定较新的 Java")
    }
}
pub(crate) trait InstallerRuntimeSource {
    fn check_task(&self) -> Result<()> {
        Ok(())
    }
    fn select(&self, config: &Config, minimum: u64) -> Result<JavaRuntime>;
    fn install(&self, config: &Config, major: u64) -> Result<JavaRuntime>;
}
impl InstallerRuntimeSource for Backend {
    fn check_task(&self) -> Result<()> {
        self.control.check()
    }
    fn select(&self, config: &Config, minimum: u64) -> Result<JavaRuntime> {
        crate::java::select(config, minimum, &self.control)
    }
    fn install(&self, config: &Config, major: u64) -> Result<JavaRuntime> {
        self.log(format!("准备安装器所需 Java {major}"));
        self.install_java(config, crate::java::component_for(major)?, major)
    }
}
pub(crate) struct InstallerJavaService<'a> {
    source: &'a dyn InstallerRuntimeSource,
}
impl<'a> InstallerJavaService<'a> {
    pub(crate) fn new(source: &'a dyn InstallerRuntimeSource) -> Self {
        Self { source }
    }
    pub(crate) fn resolve(
        &self,
        config: &Config,
        requirement: &InstallerJavaRequirement,
    ) -> Result<JavaRuntime> {
        self.source.check_task()?;
        let selected = self
            .source
            .select(config, requirement.minimum)
            .and_then(|runtime| requirement.check(runtime));
        self.source.check_task()?;
        match selected {
            Ok(runtime) => Ok(runtime),
            Err(error) if config.java != "auto" => {
                Err(error).context("指定的 Java 无法用于安装，不会自动替换手动路径")
            }
            Err(local_error) => {
                let major = requirement
                    .provision_major()
                    .with_context(|| format!("本机 Java 不满足安装要求：{local_error:#}"))?;
                let installed = self.source.install(config, major);
                self.source.check_task()?;
                let runtime = installed.with_context(|| {
                    format!(
                        "本机 Java 不满足安装要求（{local_error:#}），自动安装 Java {major} 也失败"
                    )
                })?;
                if runtime.major() != major {
                    bail!(
                        "安装器 Java 下载结果不符：预期 {major}，实际 {}",
                        runtime.major()
                    );
                }
                requirement.check(runtime)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        io::Write,
    };
    fn class_header(java: u16, minor: u16) -> Vec<u8> {
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
        bytes.extend(minor.to_be_bytes());
        bytes.extend((java + 44).to_be_bytes());
        bytes
    }
    fn jar(path: &Path, manifest: &[u8], name: &str, header: &[u8]) {
        let mut archive = zip::ZipWriter::new(fs::File::create(path).unwrap());
        archive
            .start_file(
                "META-INF/MANIFEST.MF",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(manifest).unwrap();
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(header).unwrap();
        archive
            .start_file(
                "META-INF/versions/25/example/Main.class",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(&class_header(25, 0)).unwrap();
        archive.finish().unwrap();
    }
    #[test]
    fn entry_bytecode_overrides_build_jdk_and_ignores_named_manifest_sections() {
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("installer.jar");
        let mut manifest = b"Manifest-Version: 1.0\r\nBuild-Jdk-Spec: 25\r\nMain-Class: example.Main\r\n\r\nName: example/Main.class\r\nMain-Class: ignored.Bad\r\n".to_vec();
        manifest.extend(vec![b'x'; 100000]);
        jar(&path, &manifest, "example/Main.class", &class_header(17, 0));
        let info = JarEntryPointSource
            .inspect(&path, &TaskControl::default())
            .unwrap();
        assert_eq!(info.main_class, "example.Main");
        assert_eq!(info.minimum, 17);
    }
    #[test]
    fn manifest_folding_supports_cr_lf_crlf_and_split_utf8() {
        for newline in ["\n", "\r\n", "\r"] {
            let manifest = format!("Manifest-Version: 1.0{newline}Main-Class: example.{newline} Main{newline}{newline}");
            assert_eq!(
                JarEntryPointSource::main_class(manifest.as_bytes()).unwrap(),
                "example.Main"
            );
        }
        let mut bytes = b"Main-Class: example.".to_vec();
        let c = "主".as_bytes();
        bytes.push(c[0]);
        bytes.extend_from_slice(b"\r\n ");
        bytes.extend_from_slice(&c[1..]);
        bytes.extend_from_slice(b"\r\n\r\n");
        assert_eq!(
            JarEntryPointSource::main_class(&bytes).unwrap(),
            "example.主"
        );
    }
    #[test]
    fn malformed_manifests_missing_entries_and_bad_class_headers_are_rejected() {
        for manifest in [
            "Manifest-Version: 1.0\n\nName: x\nMain-Class: wrong.Main\n",
            "Main-Class: example.Main\nmain-class: other.Main\n",
            "Main-Class: ../escape\n",
            "Main-Class: example..Main\n",
            " orphan-continuation\n",
            "Main-Class:bad-spacing\n",
        ] {
            assert!(
                JarEntryPointSource::main_class(manifest.as_bytes()).is_err(),
                "{manifest}"
            );
        }
        let oversized = format!("Main-Class: {}\n", "x".repeat(MAX_MANIFEST_MAIN as usize));
        assert!(JarEntryPointSource::main_class(oversized.as_bytes()).is_err());
        let workspace = crate::test_support::Workspace::new();
        let path = workspace.path.join("installer.jar");
        for header in [
            vec![0; 8],
            class_header(17, u16::MAX),
            class_header(17, 1),
            vec![0xca, 0xfe],
        ] {
            jar(
                &path,
                b"Main-Class: example.Main\n\n",
                "example/Main.class",
                &header,
            );
            assert!(JarEntryPointSource
                .inspect(&path, &TaskControl::default())
                .is_err());
        }
        jar(
            &path,
            b"Main-Class: missing.Main\n\n",
            "example/Main.class",
            &class_header(8, 0),
        );
        assert!(JarEntryPointSource
            .inspect(&path, &TaskControl::default())
            .is_err());
        let control = TaskControl::default();
        control.cancel();
        assert!(JarEntryPointSource
            .inspect(Path::new("missing.jar"), &control)
            .err()
            .unwrap()
            .to_string()
            .contains("取消"));
    }
    struct JarSource(u64);
    impl RunnableJarSource for JarSource {
        fn inspect(&self, _: &Path, _: &TaskControl) -> Result<RunnableJarInfo> {
            Ok(RunnableJarInfo {
                main_class: "fixture.Main".into(),
                minimum: self.0,
            })
        }
    }
    fn requirement(installer: u64, game: u64) -> InstallerJavaRequirement {
        InstallerJavaRequirement::inspect(
            &JarSource(installer),
            Path::new("unused.jar"),
            game,
            &TaskControl::default(),
        )
        .unwrap()
    }
    fn runtime(major: u64) -> JavaRuntime {
        JavaRuntime {
            path: format!("java-{major}").into(),
            version: crate::java_version::JavaVersion::parse(&major.to_string()).unwrap(),
            architecture: crate::java_architecture::JavaArchitecture::X64,
            description: String::new(),
        }
    }
    #[derive(Default)]
    struct Source {
        local: Option<u64>,
        downloaded: Option<u64>,
        installs: RefCell<Vec<u64>>,
        minimum: Cell<u64>,
        control: TaskControl,
        cancel_select: bool,
        cancel_install: bool,
    }
    impl InstallerRuntimeSource for Source {
        fn check_task(&self) -> Result<()> {
            self.control.check()
        }
        fn select(&self, _: &Config, minimum: u64) -> Result<JavaRuntime> {
            self.minimum.set(minimum);
            if self.cancel_select {
                self.control.cancel();
            }
            self.local.map(runtime).context("local unavailable")
        }
        fn install(&self, _: &Config, major: u64) -> Result<JavaRuntime> {
            self.installs.borrow_mut().push(major);
            if self.cancel_install {
                self.control.cancel();
            }
            self.downloaded.map(runtime).context("download unavailable")
        }
    }
    #[test]
    fn combines_game_and_installer_floors_and_reuses_any_compatible_local_java() {
        for (installer, game, expected) in [(8, 21, 21), (21, 8, 21), (17, 16, 17), (8, 8, 8)] {
            let source = Source {
                local: Some(25),
                ..Default::default()
            };
            let selected = InstallerJavaService::new(&source)
                .resolve(&Config::default(), &requirement(installer, game))
                .unwrap();
            assert_eq!(selected.major(), 25);
            assert_eq!(source.minimum.get(), expected);
            assert!(source.installs.borrow().is_empty());
        }
        assert!(InstallerJavaRequirement::inspect(
            &JarSource(0),
            Path::new("unused"),
            8,
            &TaskControl::default()
        )
        .is_err());
        assert!(InstallerJavaRequirement::inspect(
            &JarSource(8),
            Path::new("unused"),
            0,
            &TaskControl::default()
        )
        .is_err());
    }
    #[test]
    fn download_uses_installer_floor_instead_of_the_older_game_runtime() {
        for (installer, game, expected) in [
            (17, 8, 17),
            (21, 8, 21),
            (11, 8, 17),
            (8, 16, 16),
            (25, 21, 25),
        ] {
            let source = Source {
                local: Some(8),
                downloaded: Some(expected),
                ..Default::default()
            };
            let selected = InstallerJavaService::new(&source)
                .resolve(&Config::default(), &requirement(installer, game))
                .unwrap();
            assert_eq!(selected.major(), expected);
            assert_eq!(*source.installs.borrow(), [expected]);
        }
    }
    #[test]
    fn explicit_paths_fail_without_installing_or_changing_configuration() {
        let source = Source {
            local: Some(8),
            downloaded: Some(21),
            ..Default::default()
        };
        let config = Config {
            java: "manual-java".into(),
            ..Default::default()
        };
        assert!(InstallerJavaService::new(&source)
            .resolve(&config, &requirement(21, 8))
            .is_err());
        assert!(source.installs.borrow().is_empty());
        assert_eq!(config.java, "manual-java");
    }
    #[test]
    fn wrong_download_or_two_failures_never_become_successful_fallbacks() {
        let source = Source {
            local: Some(8),
            downloaded: Some(8),
            ..Default::default()
        };
        assert!(InstallerJavaService::new(&source)
            .resolve(&Config::default(), &requirement(17, 8))
            .is_err());
        let source = Source::default();
        let error = InstallerJavaService::new(&source)
            .resolve(&Config::default(), &requirement(21, 8))
            .err()
            .unwrap();
        let error = format!("{error:#}");
        assert!(error.contains("local unavailable") && error.contains("download unavailable"));
        let source = Source {
            local: None,
            downloaded: Some(25),
            ..Default::default()
        };
        assert!(InstallerJavaService::new(&source)
            .resolve(&Config::default(), &requirement(26, 21))
            .is_err());
        assert!(source.installs.borrow().is_empty());
        let source = Source {
            local: Some(26),
            ..Default::default()
        };
        assert_eq!(
            InstallerJavaService::new(&source)
                .resolve(&Config::default(), &requirement(26, 21))
                .unwrap()
                .major(),
            26
        );
    }
    #[test]
    fn cancellation_after_selection_or_provisioning_is_authoritative() {
        let source = Source {
            local: Some(8),
            downloaded: Some(21),
            cancel_select: true,
            ..Default::default()
        };
        assert!(InstallerJavaService::new(&source)
            .resolve(&Config::default(), &requirement(21, 8))
            .err()
            .unwrap()
            .to_string()
            .contains("取消"));
        assert!(source.installs.borrow().is_empty());
        let source = Source {
            downloaded: Some(21),
            cancel_install: true,
            ..Default::default()
        };
        assert!(InstallerJavaService::new(&source)
            .resolve(&Config::default(), &requirement(21, 8))
            .err()
            .unwrap()
            .to_string()
            .contains("取消"));
        assert_eq!(*source.installs.borrow(), [21]);
    }
    #[test]
    #[ignore = "reads cached official installers; never executes Java"]
    fn cached_forge_and_neoforge_entry_points_are_java_eight_even_for_java_twenty_one_games() {
        for file in [
            "forge-1.21.1-52.1.20-installer.jar",
            "neoforge-21.1.264-installer.jar",
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".tools/smoke-game/tui-mc-installers")
                .join(file);
            let requirement = InstallerJavaRequirement::inspect(
                &JarEntryPointSource,
                &path,
                21,
                &TaskControl::default(),
            )
            .unwrap();
            assert_eq!(requirement.installer_minimum, 8);
            assert_eq!(requirement.minimum, 21);
            assert_eq!(
                requirement.main_class,
                "net.minecraftforge.installer.SimpleInstaller"
            );
        }
    }
}
