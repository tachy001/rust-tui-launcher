use anyhow::{bail, Context, Result};

/// Architecture of the JVM process, which can differ from the launcher under emulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JavaArchitecture {
    X86,
    X64,
    Arm,
    Arm64,
    Riscv64,
    Ppc64,
    Ppc64Le,
    S390x,
}
impl JavaArchitecture {
    pub(crate) const ALL: [Self; 8] = [
        Self::X86,
        Self::X64,
        Self::Arm,
        Self::Arm64,
        Self::Riscv64,
        Self::Ppc64,
        Self::Ppc64Le,
        Self::S390x,
    ];
    pub(crate) fn parse(name: &str) -> Result<Self> {
        Ok(match name.trim().to_ascii_lowercase().as_str() {
            "x86" | "i386" | "i486" | "i586" | "i686" => Self::X86,
            "amd64" | "x86_64" | "x86-64" | "x64" => Self::X64,
            "arm" | "arm32" | "aarch32" | "armv7l" => Self::Arm,
            "aarch64" | "arm64" => Self::Arm64,
            "riscv64" => Self::Riscv64,
            "ppc64" => Self::Ppc64,
            "ppc64le" | "powerpc64le" => Self::Ppc64Le,
            "s390x" => Self::S390x,
            _ => bail!("无法识别 Java 架构：{name}，请指定受支持的 Java"),
        })
    }
    pub(crate) fn rule_name(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X64 => "x86_64",
            Self::Arm => "arm",
            Self::Arm64 => "aarch64",
            Self::Riscv64 => "riscv64",
            Self::Ppc64 => "ppc64",
            Self::Ppc64Le => "ppc64le",
            Self::S390x => "s390x",
        }
    }
    pub(crate) fn bits(self) -> &'static str {
        match self {
            Self::X86 | Self::Arm => "32",
            _ => "64",
        }
    }
    pub(crate) fn is_launcher_arch(self) -> bool {
        Self::parse(std::env::consts::ARCH).ok() == Some(self)
    }
    pub(crate) fn from_output(output: &str) -> Result<Self> {
        let mut architecture = None;
        let mut model = None;
        for line in output.lines() {
            let Some((name, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            match name.trim() {
                "os.arch" => {
                    let parsed = Self::parse(value)?;
                    if architecture.is_some_and(|previous| previous != parsed) {
                        bail!("Java 检测返回互相冲突的 os.arch");
                    }
                    architecture = Some(parsed);
                }
                "sun.arch.data.model" | "com.ibm.vm.bitmode" => {
                    if !matches!(value, "32" | "64")
                        || model.is_some_and(|previous| previous != value)
                    {
                        bail!("Java 检测返回无效或冲突的位数：{value}");
                    }
                    model = Some(value);
                }
                _ => {}
            }
        }
        let architecture = architecture
            .context("Java 检测未返回 os.arch；请使用支持 -XshowSettings:properties 的 Java")?;
        if model.is_some_and(|bits| bits != architecture.bits()) {
            bail!("Java 架构与位数不一致，不能确定 native 库目标平台");
        }
        Ok(architecture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifies_process_architecture_without_assuming_launcher_bitness() {
        for (name, architecture, bits) in [
            ("i386", JavaArchitecture::X86, "32"),
            ("amd64", JavaArchitecture::X64, "64"),
            ("aarch64", JavaArchitecture::Arm64, "64"),
            ("armv7l", JavaArchitecture::Arm, "32"),
            ("ppc64le", JavaArchitecture::Ppc64Le, "64"),
        ] {
            let output = format!("Property settings:\n os.arch = {name}\n sun.arch.data.model = {bits}\nopenjdk version \"21.0.7\"");
            assert_eq!(
                JavaArchitecture::from_output(&output).unwrap(),
                architecture
            );
            assert_eq!(architecture.bits(), bits);
        }
        assert_eq!(
            JavaArchitecture::parse("ARM64").unwrap(),
            JavaArchitecture::Arm64
        );
        assert_eq!(
            JavaArchitecture::from_output("os.arch = x86_64").unwrap(),
            JavaArchitecture::X64
        );
    }
    #[test]
    fn rejects_missing_unknown_or_contradictory_properties() {
        for output in [
            "openjdk version \"21\"\n64-Bit Server VM",
            "os.arch = mystery\nsun.arch.data.model = 64",
            "os.arch = amd64\nsun.arch.data.model = 32",
            "os.arch = x86\nos.arch = aarch64",
            "os.arch = arm64\nsun.arch.data.model = invalid",
            "os.arch = arm64\nsun.arch.data.model = 64\ncom.ibm.vm.bitmode = 32",
        ] {
            assert!(JavaArchitecture::from_output(output).is_err(), "{output}");
        }
    }
}
