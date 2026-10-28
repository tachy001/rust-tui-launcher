use crate::java_architecture::JavaArchitecture;
use anyhow::{bail, Context, Result};

/// Validate effective heap settings after all JVM argument sources are combined.
pub(crate) struct JvmMemory {
    maximum: Option<u64>,
    initial: Option<u64>,
}

impl JvmMemory {
    pub(crate) fn from_arguments(arguments: &[String]) -> Result<Self> {
        let mut memory = Self {
            maximum: None,
            initial: None,
        };
        for group in crate::jvm_argument_service::argument_groups(arguments) {
            let argument = &group[0];
            let setting = if let Some(value) = argument
                .strip_prefix("-Xmx")
                .or_else(|| argument.strip_prefix("-XX:MaxHeapSize="))
            {
                Some((&mut memory.maximum, value))
            } else {
                argument
                    .strip_prefix("-Xms")
                    .or_else(|| argument.strip_prefix("-XX:InitialHeapSize="))
                    .map(|value| (&mut memory.initial, value))
            };
            if let Some((target, value)) = setting {
                *target = Some(Self::bytes(value).with_context(|| {
                    format!("无效 JVM 内存参数 {argument}，请使用整数及可选 K / M / G 单位")
                })?);
            }
        }
        if memory.maximum == Some(0) {
            bail!("JVM 最大堆内存必须大于 0，请检查 -Xmx / MaxHeapSize");
        }
        if let (Some(initial), Some(maximum)) = (memory.initial, memory.maximum) {
            if initial > maximum {
                bail!("JVM 初始堆内存大于最大堆内存，请检查 -Xms / -Xmx 及自定义 JVM 参数");
            }
        }
        Ok(memory)
    }

    fn bytes(value: &str) -> Result<u64> {
        let (digits, multiplier) = match value.as_bytes().last() {
            Some(b'k' | b'K') => (&value[..value.len() - 1], 1_u64 << 10),
            Some(b'm' | b'M') => (&value[..value.len() - 1], 1_u64 << 20),
            Some(b'g' | b'G') => (&value[..value.len() - 1], 1_u64 << 30),
            _ => (value, 1),
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("堆内存大小必须是非负整数");
        }
        digits
            .parse::<u64>()?
            .checked_mul(multiplier)
            .context("堆内存大小溢出")
    }

    pub(crate) fn warning(&self, architecture: JavaArchitecture) -> Option<&'static str> {
        (architecture.bits() == "32" && self.maximum.is_some_and(|size| size > 1536 * 1024 * 1024))
            .then_some("当前使用 32 位 Java 且最大堆超过 1536 MiB，可能无法分配内存；建议选择兼容的 64 位 Java 或降低内存。实际限制取决于系统和 JVM。")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<JvmMemory> {
        JvmMemory::from_arguments(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn checks_effective_overrides_and_aliases_in_order() {
        let memory = parse(&["-Xmx512M", "-cp", "-Xmxinvalid", "--module-path", "-Xms2g"]).unwrap();
        assert_eq!(memory.maximum, Some(512 << 20));
        assert_eq!(memory.initial, None);
        let memory = parse(&["-Xmx512M", "-XX:MaxHeapSize=2g", "-Xms1024m"]).unwrap();
        assert_eq!(memory.maximum, Some(2 << 30));
        assert!(memory.warning(JavaArchitecture::X86).is_some());
        assert!(memory.warning(JavaArchitecture::Arm).is_some());
        assert!(memory.warning(JavaArchitecture::X64).is_none());
        let memory = parse(&["-Xmx2G", "-XX:InitialHeapSize=1G", "-Xmx1024m"]).unwrap();
        assert!(memory.warning(JavaArchitecture::X86).is_none());
        assert!(parse(&["-Xms2g", "-Xmx1G"]).is_err());
        assert!(parse(&["-Xms2g", "-Xmx1G", "-XX:InitialHeapSize=0"]).is_ok());
    }

    #[test]
    fn validates_units_zero_and_overflow_without_rewriting_arguments() {
        for value in [
            "",
            "-1M",
            "+1M",
            "1.5G",
            "2GB",
            "18446744073709551615G",
            "１２M",
        ] {
            assert!(parse(&[&format!("-Xmx{value}")]).is_err(), "{value}");
        }
        assert!(parse(&["-Xmx0"]).is_err());
        assert!(parse(&["-Xmx1073741824", "-Xms1024K"]).is_ok());
        assert!(parse(&["-Xms0", "-Dexample=-Xmxbroken"]).is_ok());
        assert!(parse(&[]).is_ok());
    }
}
