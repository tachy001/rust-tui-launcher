//! Own process containment before executing application code. No PID-tree scan
//! or shell kill command: those can race with exit, reparenting and PID reuse.
use anyhow::{bail, Context, Result};
use std::{
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct SpawnCleanupUnconfirmed(pub(crate) String);
impl std::fmt::Display for SpawnCleanupUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "启动失败且进程清理未确认：{}", self.0)
    }
}
impl std::error::Error for SpawnCleanupUnconfirmed {}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        io,
        mem::size_of,
        os::windows::{
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
            process::CommandExt,
        },
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
                JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject, TerminateJobObject,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{
                GetProcessIdOfThread, OpenThread, ResumeThread, CREATE_SUSPENDED,
                THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
            },
        },
    };

    pub(crate) struct ProcessTree {
        job: OwnedHandle,
    }
    impl ProcessTree {
        pub(crate) fn spawn(command: &mut Command) -> Result<(Child, Self)> {
            // Anonymous, non-inheritable, no breakaway flags. Descendants stay
            // in this job, even when their immediate parent has already exited.
            let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error()).context("创建进程任务对象失败");
            }
            let mut tree = Self {
                job: unsafe { OwnedHandle::from_raw_handle(raw) },
            };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    raw,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error()).context("配置进程任务对象失败");
            }
            // std's atomic job attribute API is nightly-only. Suspend before
            // user code runs, assign the job, then resume the sole initial
            // thread. Ambiguous thread snapshots fail closed, never guess.
            command.creation_flags(CREATE_SUSPENDED);
            let mut child = command.spawn().context("创建挂起进程失败")?;
            let result = (|| -> Result<()> {
                if unsafe { AssignProcessToJobObject(raw, child.as_raw_handle()) } == 0 {
                    return Err(io::Error::last_os_error())
                        .context("进程无法加入任务对象，已取消启动");
                }
                resume_initial_thread(child.id())
            })();
            if let Err(error) = result {
                let cleanup = (|| -> Result<()> {
                    if child.try_wait()?.is_none() {
                        child.kill()?;
                    }
                    tree.stop(&mut child)
                })();
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(error).context(SpawnCleanupUnconfirmed(format!("{cleanup:#}")))
                    }
                };
            }
            Ok((child, tree))
        }
        pub(crate) fn exited(&self, child: &mut Child) -> Result<bool> {
            Ok(child.try_wait()?.is_some())
        }
        pub(crate) fn stop(&mut self, child: &mut Child) -> Result<()> {
            if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error()).context("终止进程任务对象失败");
            }
            let deadline = Instant::now() + STOP_TIMEOUT;
            loop {
                let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
                if unsafe {
                    QueryInformationJobObject(
                        self.job.as_raw_handle(),
                        JobObjectBasicAccountingInformation,
                        (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                        size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                        ptr::null_mut(),
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error()).context("确认任务对象进程退出失败");
                }
                if info.ActiveProcesses == 0 && child.try_wait()?.is_some() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("进程任务对象仍有活动进程，拒绝确认停止");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn resume_initial_thread(pid: u32) -> Result<()> {
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error()).context("读取挂起进程线程失败");
        }
        let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut found = None;
        let mut available = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while available != 0 {
            if entry.th32OwnerProcessID == pid && found.replace(entry.th32ThreadID).is_some() {
                bail!("挂起进程的初始线程不唯一，取消启动");
            }
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            available = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
        }
        if io::Error::last_os_error().raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
            return Err(io::Error::last_os_error()).context("枚举挂起进程线程失败");
        }
        let tid = found.context("未找到挂起进程的初始线程")?;
        let raw = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                0,
                tid,
            )
        };
        if raw.is_null() {
            return Err(io::Error::last_os_error()).context("打开挂起线程失败");
        }
        let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
        // A snapshot contains IDs, not owned references. Validate ownership
        // after opening to reject a reused thread ID following external exit.
        if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != pid {
            bail!("挂起线程所属进程已改变，取消启动");
        }
        let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
        if previous == u32::MAX {
            return Err(io::Error::last_os_error()).context("恢复挂起线程失败");
        }
        if previous != 1 {
            bail!("初始线程挂起状态异常，取消启动");
        }
        Ok(())
    }
}

#[cfg(unix)]
mod native {
    use super::*;
    use std::{io, os::unix::process::CommandExt};
    pub(crate) struct ProcessTree {
        group: libc::pid_t,
        signalled: bool,
    }
    impl ProcessTree {
        pub(crate) fn spawn(command: &mut Command) -> Result<(Child, Self)> {
            let child = command
                .process_group(0)
                .spawn()
                .context("创建独立进程组失败")?;
            let group = child.id() as libc::pid_t;
            Ok((
                child,
                Self {
                    group,
                    signalled: false,
                },
            ))
        }
        pub(crate) fn exited(&self, child: &mut Child) -> Result<bool> {
            // Leave the leader unreaped until we signal its group, pinning its
            // PID so an unrelated new process group can never be targeted.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error()).context("检查进程组主进程失败");
            }
            Ok(unsafe { info.si_pid() } != 0)
        }
        pub(crate) fn stop(&mut self, child: &mut Child) -> Result<()> {
            if !self.signalled {
                let result = unsafe { libc::kill(-self.group, libc::SIGKILL) };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH)
                        && !(cfg!(target_os = "macos")
                            && error.raw_os_error() == Some(libc::EPERM))
                    {
                        return Err(error).context("终止进程组失败");
                    }
                }
                self.signalled = true;
            }
            let deadline = Instant::now() + STOP_TIMEOUT;
            loop {
                let reaped = child.try_wait().context("回收进程组主进程失败")?.is_some();
                if unsafe { libc::kill(-self.group, 0) } != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) && reaped {
                        return Ok(());
                    }
                    if cfg!(target_os = "macos")
                        && error.raw_os_error() == Some(libc::EPERM)
                        && reaped
                    {
                        return Ok(());
                    }
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error).context("确认进程组退出失败");
                    }
                }
                if Instant::now() >= deadline {
                    bail!("进程组仍存在，拒绝确认停止；可能有未回收的子进程");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

pub(crate) use native::ProcessTree;
