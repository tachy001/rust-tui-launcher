use crate::{backend::Backend, config::Config, java::JavaRuntime};
use anyhow::{bail, Context, Result};
use serde_json::Value;

/// A recommended major version can also have mandatory loader compatibility limits.
pub struct JavaRequirement {
    recommended: u64,
    maximum: Option<JavaCeiling>,
    profile: Option<Value>,
}
struct JavaCeiling {
    major: u64,
    release: Option<crate::java_version::JavaVersion>,
    component: &'static str,
}
impl JavaCeiling {
    fn major(major: u64, component: &'static str) -> Self {
        Self {
            major,
            release: None,
            component,
        }
    }
    fn allows(&self, runtime: &JavaRuntime) -> bool {
        runtime.major() <= self.major
            && self.release.as_ref().is_none_or(|release| {
                runtime.version.compare_release(release) != std::cmp::Ordering::Greater
            })
    }
    fn limit(&self) -> String {
        self.release
            .as_ref()
            .map_or_else(|| self.major.to_string(), ToString::to_string)
    }
    fn stricter_than(&self, other: &Self) -> bool {
        self.major < other.major
            || (self.major == other.major
                && match (&self.release, &other.release) {
                    (Some(first), Some(second)) => {
                        first.compare_release(second) == std::cmp::Ordering::Less
                    }
                    (Some(_), None) => true,
                    _ => false,
                })
    }
}
impl JavaRequirement {
    fn restrict(&mut self, ceiling: JavaCeiling) {
        if self
            .maximum
            .as_ref()
            .is_none_or(|current| ceiling.stricter_than(current))
        {
            self.maximum = Some(ceiling);
        }
    }
    fn forge_ceiling(meta: &Value) -> Result<Option<JavaCeiling>> {
        if !matches!(
            meta["mainClass"].as_str(),
            Some("cpw.mods.modlauncher.Launcher" | "cpw.mods.bootstraplauncher.BootstrapLauncher")
        ) {
            return Ok(None);
        }
        let mut found = None;
        for library in meta["libraries"].as_array().into_iter().flatten() {
            if !crate::metadata::allowed(library) {
                continue;
            }
            let Some(name) = library["name"].as_str() else {
                continue;
            };
            let (coordinate, extension) = name.split_once('@').unwrap_or((name, "jar"));
            if extension != "jar" {
                continue;
            }
            let parts: Vec<_> = coordinate.split(':').collect();
            if !(3..=4).contains(&parts.len())
                || parts[0] != "net.minecraftforge"
                || !matches!(
                    parts[1],
                    "forge" | "fmlloader" | "fmlcore" | "javafmllanguage" | "mclanguage"
                )
            {
                continue;
            }
            let text = match parts[2].split_once('-') {
                Some((game, forge))
                    if game.starts_with("1.")
                        && game.split('.').all(|part| {
                            !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit())
                        }) =>
                {
                    forge
                }
                Some(_) => continue,
                None => parts[2],
            };
            let Ok(numbers) = text
                .split('.')
                .map(str::parse::<u64>)
                .collect::<std::result::Result<Vec<_>, _>>()
            else {
                continue;
            };
            let Ok(version): std::result::Result<[u64; 3], _> = numbers.try_into() else {
                continue;
            };
            if found.is_some_and(|previous| previous != version) {
                bail!("当前平台包含互相冲突的 Forge 版本，请修复实例依赖");
            }
            found = Some(version);
        }
        let Some(version) = found else {
            return Ok(None);
        };
        let ceiling = if ([34, 0, 0]..=[36, 2, 25]).contains(&version) {
            JavaCeiling {
                major: 8,
                release: Some(crate::java_version::JavaVersion::parse("8u320")?),
                component: "Forge 34.0.0–36.2.25",
            }
        } else if ([36, 2, 26]..[37, 0, 0]).contains(&version) {
            JavaCeiling::major(23, "Forge 36.2.26–36.x")
        } else if ([37, 0, 0]..=[37, 0, 79]).contains(&version) {
            JavaCeiling::major(16, "Forge 37.0.0–37.0.79")
        } else if ([45, 0, 21]..=[45, 0, 65]).contains(&version) {
            JavaCeiling::major(19, "Forge 45.0.21–45.0.65")
        } else if ([45, 0, 66]..=[47, 4, 8]).contains(&version) {
            JavaCeiling::major(21, "Forge 45.0.66–47.4.8")
        } else {
            return Ok(None);
        };
        Ok(Some(ceiling))
    }
    fn library_version(library: &Value, group: &str, artifact: &str) -> Option<Vec<u64>> {
        if !crate::metadata::allowed(library) {
            return None;
        }
        let name = library["name"].as_str()?;
        let parts: Vec<_> = name.split(':').collect();
        if parts.len() != 3
            || parts[0] != group
            || parts[1] != artifact
            || !parts[2]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            return None;
        }
        parts[2]
            .split('.')
            .map(str::parse::<u64>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()
    }
    pub fn from_metadata(meta: &Value) -> Result<Self> {
        // A host-only conflict must not discard a valid JVM target (e.g. Rosetta).
        // Unconditional malformed metadata / incompatible limits still fail here.
        let mut requirement = match Self::from_profile(meta) {
            Ok(requirement) => requirement,
            Err(host_error) => crate::java_architecture::JavaArchitecture::ALL
                .into_iter()
                .find_map(|arch| {
                    let profile = crate::launch_platform::LaunchPlatform::new(arch).profile(meta);
                    Self::from_profile(&profile).ok()
                })
                .ok_or(host_error)?,
        };
        requirement.profile = Some(meta.clone());
        Ok(requirement)
    }
    fn from_profile(meta: &Value) -> Result<Self> {
        let recommended = match meta.pointer("/javaVersion/majorVersion") {
            None => 8,
            Some(value) => value
                .as_u64()
                .filter(|major| *major > 0)
                .ok_or_else(|| anyhow::anyhow!("版本元数据中的 Java majorVersion 必须为正整数"))?,
        };
        // HMCL's LAUNCH_WRAPPER constraint: old LaunchWrapper assumes URLClassLoader.
        // Inspect the launcher and library identity, not an instance's editable name.
        let legacy_wrapper = meta["mainClass"] == "net.minecraft.launchwrapper.Launch"
            && meta["libraries"].as_array().is_some_and(|libraries| {
                libraries.iter().any(|library| {
                    let Some(version) =
                        Self::library_version(library, "net.minecraft", "launchwrapper")
                    else {
                        return false;
                    };
                    version.first().is_some_and(|major| {
                        *major == 0 || (*major == 1 && version.get(1).copied().unwrap_or(0) < 13)
                    })
                })
            });
        // PCL GetJavaRequirement: Fabric Loader before 0.17.0 ships Mixin/ASM
        // that cannot handle Java 25. An inert library must not limit vanilla.
        let old_fabric = matches!(
            meta["mainClass"].as_str(),
            Some(
                "net.fabricmc.loader.impl.launch.knot.KnotClient"
                    | "net.fabricmc.loader.launch.knot.KnotClient"
            )
        ) && meta["libraries"].as_array().is_some_and(|libraries| {
            libraries.iter().any(|library| {
                Self::library_version(library, "net.fabricmc", "fabric-loader").is_some_and(
                    |version| {
                        version.first() == Some(&0) && version.get(1).copied().unwrap_or(0) < 17
                    },
                )
            })
        });
        let mut requirement = Self {
            recommended,
            maximum: None,
            profile: None,
        };
        if legacy_wrapper {
            requirement.restrict(JavaCeiling::major(8, "旧 LaunchWrapper"));
        }
        if old_fabric {
            requirement.restrict(JavaCeiling::major(24, "Fabric Loader 0.17.0 之前的版本"));
        }
        if let Some(ceiling) = Self::forge_ceiling(meta)? {
            requirement.restrict(ceiling);
        }
        if let Some(maximum) = &requirement.maximum {
            if recommended > maximum.major {
                bail!("版本 Java 要求冲突：元数据要求 Java {recommended}，{} 最高支持 Java {}；请升级加载器或修正实例配置", maximum.component, maximum.major);
            }
        }
        Ok(requirement)
    }
    pub fn recommended(&self) -> u64 {
        self.recommended
    }
    fn check_runtime(&self, runtime: &JavaRuntime) -> Result<()> {
        if let Some(profile) = &self.profile {
            let profile =
                crate::launch_platform::LaunchPlatform::new(runtime.architecture).profile(profile);
            return Self::from_profile(&profile)?.check_runtime(runtime);
        }
        if runtime.major() < self.recommended {
            bail!(
                "需要 Java {}，当前 Java {}",
                self.recommended,
                runtime.major()
            );
        }
        if let Some(maximum) = &self.maximum {
            if !maximum.allows(runtime) {
                bail!(
                    "{} 不兼容 Java {}，最高支持 Java {}；建议为此实例使用 Java {}",
                    maximum.component,
                    runtime.version,
                    maximum.limit(),
                    self.recommended
                );
            }
        }
        Ok(())
    }
    pub(crate) fn check(&self, runtime: JavaRuntime) -> Result<JavaRuntime> {
        self.check_runtime(&runtime)?;
        Ok(runtime)
    }
    pub(crate) fn choose(&self, runtimes: Vec<JavaRuntime>) -> Result<JavaRuntime> {
        let mut first_error = None;
        let compatible = runtimes
            .into_iter()
            .filter(|runtime| match self.check_runtime(runtime) {
                Ok(()) => true,
                Err(error) => {
                    first_error.get_or_insert(error);
                    false
                }
            })
            .collect();
        crate::java::choose(compatible, self.recommended).with_context(|| match first_error {
            Some(error) => format!(
                "本机没有满足 Java {} 与目标平台加载器要求的运行时：{error:#}",
                self.recommended
            ),
            None => format!("本机没有满足 Java {} 要求的运行时", self.recommended),
        })
    }
    #[cfg(test)]
    fn minimum(recommended: u64) -> Self {
        Self {
            recommended,
            maximum: None,
            profile: None,
        }
    }
}

/// Runtime discovery and provisioning are replaceable independently of policy.
pub trait JavaRuntimeSource {
    fn check_task(&self) -> Result<()> {
        Ok(())
    }
    fn select(&self, config: &Config, requirement: &JavaRequirement) -> Result<JavaRuntime>;
    fn install(&self, config: &Config, instance: &str, required: u64) -> Result<JavaRuntime>;
}

impl JavaRuntimeSource for Backend {
    fn check_task(&self) -> Result<()> {
        self.control.check()
    }
    fn select(&self, config: &Config, requirement: &JavaRequirement) -> Result<JavaRuntime> {
        if config.java == "auto" {
            requirement.choose(crate::java::discover(config, &self.control)?)
        } else {
            crate::java::select(config, requirement.recommended(), &self.control)
        }
    }
    fn install(&self, config: &Config, instance: &str, _required: u64) -> Result<JavaRuntime> {
        self.install_java_for_version(config, instance)
    }
}

pub struct RuntimeResolution {
    runtime: JavaRuntime,
    warning: Option<String>,
}
impl RuntimeResolution {
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
    pub fn into_runtime(self) -> JavaRuntime {
        self.runtime
    }
}

/// Owns the explicit-path / recommended-runtime / fallback policy.
pub struct JavaRuntimeService<'a> {
    source: &'a dyn JavaRuntimeSource,
}
impl<'a> JavaRuntimeService<'a> {
    pub fn new(source: &'a dyn JavaRuntimeSource) -> Self {
        Self { source }
    }
    pub fn resolve(
        &self,
        config: &Config,
        instance: &str,
        requirement: JavaRequirement,
    ) -> Result<RuntimeResolution> {
        self.source.check_task()?;
        let required = requirement.recommended();
        let selected = self
            .source
            .select(config, &requirement)
            .and_then(|runtime| requirement.check(runtime));
        self.source.check_task()?;
        if config.java != "auto"
            || selected
                .as_ref()
                .is_ok_and(|runtime| runtime.major() == required)
        {
            return Ok(RuntimeResolution {
                runtime: selected?,
                warning: None,
            });
        }
        let installed = self
            .source
            .install(config, instance, required)
            .and_then(|runtime| {
                if runtime.major() != required {
                    bail!(
                        "下载后的 Java 版本不符：预期 {required}，实际 {}",
                        runtime.major()
                    );
                }
                requirement.check(runtime)
            });
        self.source.check_task()?;
        match installed {
            Ok(runtime) => Ok(RuntimeResolution {
                runtime,
                warning: None,
            }),
            Err(error) => match selected {
                Ok(runtime) => {
                    let warning = format!(
                        "安装建议 Java 失败，将尝试本机 Java {}：{error:#}",
                        runtime.major()
                    );
                    Ok(RuntimeResolution {
                        runtime,
                        warning: Some(warning),
                    })
                }
                Err(local_error) => Err(error).with_context(|| format!("本机没有满足要求的 Java（{local_error:#}），且安装建议 Java {required} 失败")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn loader_limits_follow_each_candidate_jvm_instead_of_launcher_architecture() {
        let mut profile = forge_profile("36.2.25", 8);
        profile["libraries"][0]["rules"] = serde_json::json!([
            {"action":"allow","os":{"arch":"aarch64"}}
        ]);
        let requirement = JavaRequirement::from_metadata(&profile).unwrap();
        let mut arm = full_runtime("8u451");
        arm.architecture = crate::java_architecture::JavaArchitecture::Arm64;
        let mut x64 = arm.clone();
        x64.architecture = crate::java_architecture::JavaArchitecture::X64;
        assert!(requirement.check(arm.clone()).is_err());
        assert!(requirement.check(x64.clone()).is_ok());
        assert_eq!(
            requirement.choose(vec![arm, x64]).unwrap().architecture,
            crate::java_architecture::JavaArchitecture::X64
        );

        profile["javaVersion"]["majorVersion"] = 17.into();
        let requirement = JavaRequirement::from_metadata(&profile).unwrap();
        let mut arm = full_runtime("17");
        arm.architecture = crate::java_architecture::JavaArchitecture::Arm64;
        let mut x64 = arm.clone();
        x64.architecture = crate::java_architecture::JavaArchitecture::X64;
        assert!(requirement.check(arm).is_err());
        assert!(requirement.check(x64).is_ok());
    }
    fn forge_profile(build: &str, java: u64) -> Value {
        serde_json::json!({"id":"renamed-instance","mainClass":"cpw.mods.modlauncher.Launcher",
            "javaVersion":{"majorVersion":java},
            "libraries":[{"name":format!("net.minecraftforge:forge:1.16.5-{build}:universal")} ]})
    }
    fn full_runtime(version: &str) -> JavaRuntime {
        JavaRuntime {
            path: format!("java-{version}").into(),
            version: crate::java_version::JavaVersion::parse(version).unwrap(),
            description: String::new(),
            architecture: crate::java_architecture::JavaArchitecture::X64,
        }
    }
    #[test]
    fn forge_update_ceiling_accepts_vendor_builds_but_rejects_newer_updates() {
        let requirement = JavaRequirement::from_metadata(&forge_profile("36.2.25", 8)).unwrap();
        for text in ["8u51", "1.8.0_312-b07", "1.8.0_320-b99"] {
            assert!(requirement.check(full_runtime(text)).is_ok(), "{text}");
        }
        for text in ["8u321", "8u451", "11", "21"] {
            assert!(requirement.check(full_runtime(text)).is_err(), "{text}");
        }
    }
    #[test]
    fn chooses_latest_compatible_update_from_all_local_candidates() {
        let requirement = JavaRequirement::from_metadata(&forge_profile("36.2.25", 8)).unwrap();
        let chosen = requirement
            .choose(
                ["8u451", "8u312", "1.8.0_320-b07", "17"]
                    .into_iter()
                    .map(full_runtime)
                    .collect(),
            )
            .unwrap();
        assert_eq!(chosen.version.to_string(), "1.8.0_320-b07");
    }
    #[test]
    fn forge_ranges_have_exact_transition_boundaries() {
        for (forge, java, allowed, denied) in [
            ("34.0.0", 8, "8u320", "8u321"),
            ("36.2.26", 8, "23", "24"),
            ("36.3.99", 8, "23", "24"),
            ("37.0.0", 16, "16", "17"),
            ("37.0.79", 16, "16", "17"),
            ("45.0.21", 17, "19", "20"),
            ("45.0.65", 17, "19", "20"),
            ("45.0.66", 17, "21", "22"),
            ("47.4.8", 17, "21", "22"),
        ] {
            let requirement = JavaRequirement::from_metadata(&forge_profile(forge, java)).unwrap();
            assert!(requirement.check(full_runtime(allowed)).is_ok(), "{forge}");
            assert!(requirement.check(full_runtime(denied)).is_err(), "{forge}");
        }
        for forge in ["33.0.0", "37.0.80", "45.0.20", "47.4.9", "36.2.25-beta"] {
            assert!(
                JavaRequirement::from_metadata(&forge_profile(forge, 8))
                    .unwrap()
                    .check(runtime(25))
                    .is_ok(),
                "{forge}"
            );
        }
    }
    #[test]
    fn forge_detection_requires_active_identity_and_rejects_conflicts() {
        let mut profile = forge_profile("36.2.25", 8);
        profile["libraries"][0]["rules"] = serde_json::json!([{"action":"disallow"}]);
        assert!(JavaRequirement::from_metadata(&profile)
            .unwrap()
            .check(runtime(21))
            .is_ok());
        profile = forge_profile("36.2.25", 8);
        profile["mainClass"] = "net.minecraft.client.main.Main".into();
        assert!(JavaRequirement::from_metadata(&profile)
            .unwrap()
            .check(runtime(21))
            .is_ok());
        profile = forge_profile("36.2.25", 8);
        profile["libraries"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name":"net.minecraftforge:fmlloader:1.16.5-36.2.26"}));
        assert!(JavaRequirement::from_metadata(&profile).is_err());
        assert!(JavaRequirement::from_metadata(&forge_profile("36.2.25", 17)).is_err());
        for name in [
            "net.minecraftforge:forge:1.16.5-36.2.25:universal@jar",
            "net.minecraftforge:fmlloader:36.2.25",
        ] {
            let mut profile = forge_profile("36.2.25", 8);
            profile["libraries"][0]["name"] = name.into();
            profile["mainClass"] = "cpw.mods.bootstraplauncher.BootstrapLauncher".into();
            assert!(JavaRequirement::from_metadata(&profile)
                .unwrap()
                .check(full_runtime("8u321"))
                .is_err());
        }
        for name in [
            "net.minecraftforge:forge:36.2.25-beta",
            "net.minecraftforge:forge:garbage-36.2.25",
            "net.minecraftforge:mergetool:36.2.25",
            "net.neoforged:forge:1.16.5-36.2.25",
        ] {
            let mut profile = forge_profile("36.2.25", 8);
            profile["libraries"][0]["name"] = name.into();
            assert!(JavaRequirement::from_metadata(&profile)
                .unwrap()
                .check(runtime(25))
                .is_ok());
        }
    }
    struct RuntimePool {
        versions: Vec<&'static str>,
        installed: &'static str,
        installs: Cell<usize>,
    }
    impl JavaRuntimeSource for RuntimePool {
        fn select(&self, config: &Config, requirement: &JavaRequirement) -> Result<JavaRuntime> {
            if config.java != "auto" {
                return Ok(full_runtime(self.versions[0]));
            }
            requirement.choose(self.versions.iter().map(|s| full_runtime(s)).collect())
        }
        fn install(&self, _: &Config, _: &str, _: u64) -> Result<JavaRuntime> {
            self.installs.set(self.installs.get() + 1);
            Ok(full_runtime(self.installed))
        }
    }
    #[test]
    fn local_compatible_update_avoids_download_and_manual_path_is_not_replaced() {
        let pool = RuntimePool {
            versions: vec!["8u451", "8u312"],
            installed: "8u51",
            installs: Cell::new(0),
        };
        let requirement = || JavaRequirement::from_metadata(&forge_profile("36.2.25", 8)).unwrap();
        let result = JavaRuntimeService::new(&pool)
            .resolve(&Config::default(), "test", requirement())
            .unwrap();
        assert_eq!(result.into_runtime().version.to_string(), "8u312");
        assert_eq!(pool.installs.get(), 0);
        let explicit = Config {
            java: "manual-java".into(),
            ..Config::default()
        };
        assert!(JavaRuntimeService::new(&pool)
            .resolve(&explicit, "test", requirement())
            .is_err());
        assert_eq!(pool.installs.get(), 0);
    }
    #[test]
    fn downloaded_runtime_must_satisfy_update_ceiling() {
        let mut pool = RuntimePool {
            versions: vec!["8u451"],
            installed: "8u451",
            installs: Cell::new(0),
        };
        let requirement = || JavaRequirement::from_metadata(&forge_profile("36.2.25", 8)).unwrap();
        let error = JavaRuntimeService::new(&pool)
            .resolve(&Config::default(), "test", requirement())
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("8u320"));
        pool.installed = "8u312";
        let result = JavaRuntimeService::new(&pool)
            .resolve(&Config::default(), "test", requirement())
            .unwrap();
        assert_eq!(result.into_runtime().version.to_string(), "8u312");
    }
    struct Source {
        local: Option<u64>,
        installed: Option<u64>,
        calls: Cell<usize>,
    }
    fn runtime(major: u64) -> JavaRuntime {
        JavaRuntime {
            path: format!("java{major}").into(),
            version: crate::java_version::JavaVersion::parse(&major.to_string()).unwrap(),
            description: String::new(),
            architecture: crate::java_architecture::JavaArchitecture::X64,
        }
    }
    impl JavaRuntimeSource for Source {
        fn select(&self, _config: &Config, _requirement: &JavaRequirement) -> Result<JavaRuntime> {
            self.local
                .map(runtime)
                .ok_or_else(|| anyhow::anyhow!("未安装 Java"))
        }
        fn install(
            &self,
            _config: &Config,
            _instance: &str,
            _required: u64,
        ) -> Result<JavaRuntime> {
            self.calls.set(self.calls.get() + 1);
            self.installed
                .map(runtime)
                .ok_or_else(|| anyhow::anyhow!("下载失败"))
        }
    }
    #[test]
    fn cancellation_during_install_does_not_turn_into_a_successful_fallback() {
        struct CancellingSource(Cell<bool>);
        impl JavaRuntimeSource for CancellingSource {
            fn check_task(&self) -> Result<()> {
                if self.0.get() {
                    bail!("任务已取消");
                }
                Ok(())
            }
            fn select(
                &self,
                _config: &Config,
                _requirement: &JavaRequirement,
            ) -> Result<JavaRuntime> {
                Ok(runtime(25))
            }
            fn install(
                &self,
                _config: &Config,
                _instance: &str,
                _required: u64,
            ) -> Result<JavaRuntime> {
                self.0.set(true);
                bail!("下载已取消");
            }
        }
        let source = CancellingSource(Cell::new(false));
        let result = JavaRuntimeService::new(&source).resolve(
            &Config::default(),
            "game",
            JavaRequirement::minimum(21),
        );
        assert_eq!(result.err().unwrap().to_string(), "任务已取消");
    }
    #[test]
    fn exact_local_and_explicit_paths_never_trigger_installation() {
        let source = Source {
            local: Some(21),
            installed: None,
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        assert_eq!(
            service
                .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
                .unwrap()
                .into_runtime()
                .major(),
            21
        );
        let explicit = Config {
            java: "custom-java".into(),
            ..Config::default()
        };
        assert!(service
            .resolve(&explicit, "game", JavaRequirement::minimum(25))
            .is_err());
        assert_eq!(source.calls.get(), 0);
    }
    #[test]
    fn auto_prefers_recommended_install_and_reports_a_safe_fallback() {
        let source = Source {
            local: Some(25),
            installed: Some(21),
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        assert_eq!(
            service
                .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
                .unwrap()
                .into_runtime()
                .major(),
            21
        );
        let failing = Source {
            installed: None,
            ..source
        };
        let resolution = JavaRuntimeService::new(&failing)
            .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
            .unwrap();
        assert!(resolution.warning().unwrap().contains("下载失败"));
        assert_eq!(resolution.into_runtime().major(), 25);
    }
    #[test]
    fn cannot_fallback_to_too_old_java_or_accept_wrong_installed_version() {
        for local in [None, Some(8)] {
            let source = Source {
                local,
                installed: Some(17),
                calls: Cell::new(0),
            };
            let error = JavaRuntimeService::new(&source)
                .resolve(&Config::default(), "game", JavaRequirement::minimum(21))
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("预期 21"));
            assert_eq!(source.calls.get(), 1);
        }
    }
    fn wrapper(version: &str) -> Value {
        serde_json::json!({
            "mainClass":"net.minecraft.launchwrapper.Launch",
            "libraries":[{"name":format!("net.minecraft:launchwrapper:{version}")}],
            "javaVersion":{"majorVersion":8}
        })
    }
    #[test]
    fn old_wrapper_cannot_fallback_to_new_java_or_silently_replace_explicit_java() {
        let source = Source {
            local: Some(21),
            installed: None,
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        assert!(service
            .resolve(
                &Config::default(),
                "renamed-instance",
                JavaRequirement::from_metadata(&wrapper("1.12")).unwrap()
            )
            .is_err());
        assert_eq!(source.calls.get(), 1);
        let explicit = Config {
            java: "custom-java".into(),
            ..Default::default()
        };
        let error = service
            .resolve(
                &explicit,
                "renamed-instance",
                JavaRequirement::from_metadata(&wrapper("1.12")).unwrap(),
            )
            .err()
            .unwrap();
        assert!(error
            .to_string()
            .contains("旧 LaunchWrapper 不兼容 Java 21"));
        assert_eq!(source.calls.get(), 1);
    }
    #[test]
    fn old_wrapper_reuses_or_installs_compatible_java_eight() {
        for local in [Some(8), Some(21), None] {
            let source = Source {
                local,
                installed: Some(8),
                calls: Cell::new(0),
            };
            let runtime = JavaRuntimeService::new(&source)
                .resolve(
                    &Config::default(),
                    "game",
                    JavaRequirement::from_metadata(&wrapper("1.12")).unwrap(),
                )
                .unwrap()
                .into_runtime();
            assert_eq!(runtime.major(), 8);
            assert_eq!(source.calls.get(), usize::from(local != Some(8)));
        }
    }
    #[test]
    fn newer_wrapper_or_different_launcher_does_not_inherit_legacy_restriction() {
        for mut meta in [wrapper("1.13"), wrapper("2.0"), wrapper("1.12")] {
            if meta["libraries"][0]["name"] == "net.minecraft:launchwrapper:1.12" {
                meta["mainClass"] = "net.minecraft.client.main.Main".into();
            }
            assert!(JavaRequirement::from_metadata(&meta)
                .unwrap()
                .check(runtime(21))
                .is_ok());
        }
    }
    #[test]
    fn conflicting_or_invalid_java_metadata_is_rejected_before_runtime_selection() {
        for major in [
            serde_json::json!(17),
            serde_json::json!(0),
            serde_json::json!("8"),
            Value::Null,
        ] {
            let mut meta = wrapper("1.12");
            meta["javaVersion"]["majorVersion"] = major;
            assert!(JavaRequirement::from_metadata(&meta).is_err());
        }
        assert_eq!(
            JavaRequirement::from_metadata(&serde_json::json!({}))
                .unwrap()
                .recommended(),
            8
        );
    }
    fn fabric(version: &str) -> Value {
        serde_json::json!({"mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient", "javaVersion":{"majorVersion":21}, "libraries":[{"name":format!("net.fabricmc:fabric-loader:{version}")}]})
    }
    #[test]
    fn old_fabric_rejects_java_25_in_explicit_and_failed_download_fallback_modes() {
        let source = Source {
            local: Some(25),
            installed: None,
            calls: Cell::new(0),
        };
        let service = JavaRuntimeService::new(&source);
        let error = service
            .resolve(
                &Config::default(),
                "renamed-fabric",
                JavaRequirement::from_metadata(&fabric("0.16.14")).unwrap(),
            )
            .err()
            .unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("Fabric Loader 0.17.0 之前的版本 不兼容 Java 25"));
        assert!(message.contains("下载失败"));
        assert_eq!(source.calls.get(), 1);
        let explicit = Config {
            java: "custom-java".into(),
            ..Default::default()
        };
        assert!(service
            .resolve(
                &explicit,
                "renamed-fabric",
                JavaRequirement::from_metadata(&fabric("0.16.14")).unwrap()
            )
            .is_err());
        assert_eq!(source.calls.get(), 1);
    }
    #[test]
    fn old_fabric_allows_compatible_fallback_and_recommended_installation() {
        for installed in [None, Some(21)] {
            let source = Source {
                local: Some(24),
                installed,
                calls: Cell::new(0),
            };
            let resolution = JavaRuntimeService::new(&source)
                .resolve(
                    &Config::default(),
                    "fabric",
                    JavaRequirement::from_metadata(&fabric("0.16.14")).unwrap(),
                )
                .unwrap();
            assert_eq!(resolution.warning().is_some(), installed.is_none());
            assert_eq!(resolution.into_runtime().major(), installed.unwrap_or(24));
        }
    }
    #[test]
    fn fabric_limits_follow_active_library_and_launcher_not_the_instance_label() {
        for (version, restricted) in [
            ("0.16.14", true),
            ("0.17.0", false),
            ("0.18.0", false),
            ("1.0.0", false),
            ("0.17.0-beta", false),
        ] {
            assert_eq!(
                JavaRequirement::from_metadata(&fabric(version))
                    .unwrap()
                    .check(runtime(25))
                    .is_err(),
                restricted
            );
        }
        let mut inactive = fabric("0.16.14");
        inactive["libraries"][0]["rules"] = serde_json::json!([{"action":"disallow"}]);
        assert!(JavaRequirement::from_metadata(&inactive)
            .unwrap()
            .check(runtime(25))
            .is_ok());
        inactive["libraries"][0]
            .as_object_mut()
            .unwrap()
            .remove("rules");
        inactive["mainClass"] = "net.minecraft.client.main.Main".into();
        assert!(JavaRequirement::from_metadata(&inactive)
            .unwrap()
            .check(runtime(25))
            .is_ok());
        let mut conflict = fabric("0.16.14");
        conflict["javaVersion"]["majorVersion"] = 25.into();
        assert!(JavaRequirement::from_metadata(&conflict).is_err());
    }
}
