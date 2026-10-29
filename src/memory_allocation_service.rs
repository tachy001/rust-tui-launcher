use crate::java_architecture::JavaArchitecture;
use anyhow::{Context, Result};

const MIB: u64 = 1024 * 1024;
pub(crate) trait AvailableMemory {
    fn bytes(&self) -> Result<u64>;
}
pub(crate) struct SystemMemory;
impl AvailableMemory for SystemMemory {
    fn bytes(&self) -> Result<u64> {
        system_available()
    }
}
pub(crate) struct MemoryAllocationService<'a> {
    source: &'a dyn AvailableMemory,
}
pub(crate) struct MemoryAllocation {
    pub(crate) megabytes: u32,
    pub(crate) warning: Option<String>,
}
impl<'a> MemoryAllocationService<'a> {
    pub(crate) fn new(source: &'a dyn AvailableMemory) -> Self {
        Self { source }
    }
    pub(crate) fn resolve(
        &self,
        automatic: bool,
        manual: u32,
        architecture: JavaArchitecture,
    ) -> MemoryAllocation {
        if !automatic {
            return MemoryAllocation {
                megabytes: manual,
                warning: None,
            };
        }
        match self.allocate(architecture) {
            Ok(megabytes) => MemoryAllocation {
                megabytes,
                warning: None,
            },
            Err(error) => MemoryAllocation {
                megabytes: manual,
                warning: Some(format!(
                    "自动内存探测失败，使用配置值 {manual} MiB：{error:#}"
                )),
            },
        }
    }
    pub(crate) fn allocate(&self, architecture: JavaArchitecture) -> Result<u32> {
        let available = self.source.bytes()?;
        anyhow::ensure!(available >= MIB, "系统未提供有效的可用物理内存");
        // HMCL reserves 512 MiB, then uses 80% below 8 GiB and 20% above.
        let usable = available.saturating_sub(512 * MIB);
        let threshold = 8192 * MIB;
        let suggested = if usable == 0 {
            available
        } else {
            (usable.min(threshold) * 4 / 5 + usable.saturating_sub(threshold) / 5).min(16384 * MIB)
        };
        // Also apply the 32-bit ceiling in the low-memory branch.
        let suggested = if architecture.bits() == "32" {
            suggested.min(768 * MIB)
        } else {
            suggested
        };
        Ok((suggested / MIB).max(1) as u32)
    }
}
#[cfg(windows)]
fn system_available() -> Result<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(std::io::Error::last_os_error()).context("读取可用物理内存失败");
    }
    Ok(status.ullAvailPhys)
}
#[cfg(target_os = "linux")]
fn system_available() -> Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    parse_meminfo(&text)
}
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(text: &str) -> Result<u64> {
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .context("缺少 MemAvailable")?;
    let mut parts = line.split_whitespace();
    let kb: u64 = parts.next().context("缺少可用内存数值")?.parse()?;
    anyhow::ensure!(parts.next() == Some("kB"), "未知内存单位");
    kb.checked_mul(1024).context("内存数值溢出")
}
#[cfg(target_os = "macos")]
fn system_available() -> Result<u64> {
    // Retain one host send right for the process lifetime, rather than obtaining
    // a new right on every launch. No helper process or textual locale parsing.
    static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    // libc recommends mach2 for this binding; retain the existing dependency
    // for this single stable ABI call, with the allowance scoped to that call.
    #[allow(deprecated)]
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let result = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            (&mut stats as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    anyhow::ensure!(
        result == libc::KERN_SUCCESS,
        "读取 Mach 内存统计失败：{result}"
    );
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    anyhow::ensure!(page_size > 0, "无法读取系统页大小");
    // XNU free_count already includes speculative_count.
    mac_available_bytes(
        u64::from(stats.free_count),
        u64::from(stats.inactive_count),
        page_size as u64,
    )
}
#[cfg(any(target_os = "macos", test))]
fn mac_available_bytes(free: u64, inactive: u64, page_size: u64) -> Result<u64> {
    anyhow::ensure!(
        page_size.is_power_of_two() && page_size >= 1024,
        "无效内存页大小"
    );
    let pages = free.checked_add(inactive).context("内存页数溢出")?;
    pages.checked_mul(page_size).context("内存数值溢出")
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn system_available() -> Result<u64> {
    anyhow::bail!("当前平台尚不支持自动内存探测")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platform_statistics_use_available_memory_and_actual_page_size() {
        assert_eq!(
            parse_meminfo("MemTotal: 99999 kB\nMemFree: 100 kB\nMemAvailable: 2048 kB\n").unwrap(),
            2 * MIB
        );
        for text in [
            "MemFree: 100 kB",
            "MemAvailable: 12 MB",
            "MemAvailable: -1 kB",
            "MemAvailable: 18446744073709551615 kB",
        ] {
            assert!(parse_meminfo(text).is_err());
        }
        for size in [4096, 16384] {
            assert_eq!(mac_available_bytes(10, 20, size).unwrap(), 30 * size);
            assert!(mac_available_bytes(u64::MAX, 1, size).is_err());
            assert!(mac_available_bytes(10, 20, 0).is_err());
        }
    }
    #[test]
    #[ignore = "reads the current host's available memory; never starts Java"]
    fn current_host_memory_probe() {
        let available = SystemMemory.bytes().unwrap();
        assert!(available >= MIB);
        println!("available memory: {} MiB", available / MIB);
    }
    struct Source(u64);
    impl AvailableMemory for Source {
        fn bytes(&self) -> Result<u64> {
            Ok(self.0 * MIB)
        }
    }
    #[test]
    fn hmcl_allocation_reserves_memory_and_caps_java_architecture() {
        for (available, expected) in [
            (256, 256),
            (512, 512),
            (1024, 409),
            (8704, 6553),
            (65536, 16384),
        ] {
            assert_eq!(
                MemoryAllocationService::new(&Source(available))
                    .allocate(JavaArchitecture::X64)
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            MemoryAllocationService::new(&Source(16384))
                .allocate(JavaArchitecture::X86)
                .unwrap(),
            768
        );
        assert!(MemoryAllocationService::new(&Source(0))
            .allocate(JavaArchitecture::X64)
            .is_err());
    }
    #[test]
    fn failed_probe_returns_error_instead_of_fabricating_a_recommendation() {
        struct Failed;
        impl AvailableMemory for Failed {
            fn bytes(&self) -> Result<u64> {
                anyhow::bail!("probe unavailable")
            }
        }
        assert!(MemoryAllocationService::new(&Failed)
            .allocate(JavaArchitecture::X64)
            .unwrap_err()
            .to_string()
            .contains("probe unavailable"));
        let fallback =
            MemoryAllocationService::new(&Failed).resolve(true, 3072, JavaArchitecture::X64);
        assert_eq!(fallback.megabytes, 3072);
        assert!(fallback.warning.unwrap().contains("probe unavailable"));
        struct MustNotProbe;
        impl AvailableMemory for MustNotProbe {
            fn bytes(&self) -> Result<u64> {
                panic!("manual mode must not probe")
            }
        }
        let manual =
            MemoryAllocationService::new(&MustNotProbe).resolve(false, 4096, JavaArchitecture::X64);
        assert_eq!(manual.megabytes, 4096);
        assert!(manual.warning.is_none());
    }
}
