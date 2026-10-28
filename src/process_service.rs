use crate::{logs::LogTail, task::TaskControl};
use anyhow::{Context, Result};
use std::{
    process::{Child, Command, ExitStatus},
    thread,
    time::Duration,
};

pub(crate) trait ProcessHandle {
    fn poll(&mut self) -> Result<Option<ExitStatus>>;
    fn terminate(&mut self) -> Result<()>;
}

pub(crate) trait ProcessOutput {
    fn read(&mut self, finished: bool) -> Result<Vec<String>>;
}
impl ProcessOutput for LogTail {
    fn read(&mut self, finished: bool) -> Result<Vec<String>> {
        if finished {
            self.finish()
        } else {
            self.read_lines()
        }
    }
}

/// Owns a child until it has exited and been reaped, including early error paths.
pub(crate) struct ManagedProcess {
    child: Child,
    tree: Option<crate::process_tree_service::ProcessTree>,
    resources: Option<Box<dyn Send>>,
}
impl ManagedProcess {
    pub(crate) fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }
    pub(crate) fn spawn(command: &mut Command) -> Result<Self> {
        let (child, tree) = crate::process_tree_service::ProcessTree::spawn(command)?;
        Ok(Self {
            child,
            tree: Some(tree),
            resources: None,
        })
    }
    /// Transfer leases before spawn so even unconfirmed spawn cleanup retains them.
    pub(crate) fn spawn_guarded(
        command: &mut Command,
        resources: impl Send + 'static,
    ) -> Result<Self> {
        match Self::spawn(command) {
            Ok(mut process) => {
                process.resources = Some(Box::new(resources));
                Ok(process)
            }
            Err(error) => {
                if error.is::<crate::process_tree_service::SpawnCleanupUnconfirmed>() {
                    std::mem::forget(resources);
                    return Err(error).context("进程停止未确认，目录锁将保留至启动器退出");
                }
                Err(error)
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn from_child(child: Child) -> Self {
        Self {
            child,
            tree: None,
            resources: None,
        }
    }
}
impl std::ops::Deref for ManagedProcess {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}
// Only fixtures that adopt an already-created child need raw wait/kill access.
// Production callers must use poll/terminate so containment is never bypassed.
#[cfg(test)]
impl std::ops::DerefMut for ManagedProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}
impl ProcessHandle for ManagedProcess {
    fn poll(&mut self) -> Result<Option<ExitStatus>> {
        if let Some(tree) = &self.tree {
            if !tree.exited(&mut self.child)? {
                return Ok(None);
            }
            self.terminate()?;
        }
        self.child.try_wait().context("读取进程退出状态失败")
    }
    fn terminate(&mut self) -> Result<()> {
        if let Some(tree) = &mut self.tree {
            tree.stop(&mut self.child)?;
            self.tree = None;
            return Ok(());
        }
        if self.child.try_wait()?.is_none() {
            if let Err(error) = self.child.kill() {
                // The process may have exited between the status check and kill.
                if self.child.try_wait()?.is_none() {
                    return Err(error).context("终止进程失败");
                }
            }
        }
        self.child.wait().context("等待进程退出失败")?;
        Ok(())
    }
}
impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if (self.tree.is_some() || !matches!(self.child.try_wait(), Ok(Some(_))))
            && self.terminate().is_err()
        {
            // Without proof of termination, releasing file leases could let
            // a second task overwrite files still used by descendants.
            // Deliberately retain them until launcher exit, not a guessed timeout.
            if let Some(resources) = self.resources.take() {
                std::mem::forget(resources);
            }
        }
    }
}

pub(crate) enum ProcessOutcome {
    Exited(ExitStatus),
    Cancelled,
}

/// Observing a log is optional; process status and explicit cancellation are authoritative.
pub(crate) struct ProcessMonitor<'a> {
    control: &'a TaskControl,
    interval: Duration,
}
impl<'a> ProcessMonitor<'a> {
    pub(crate) fn new(control: &'a TaskControl) -> Self {
        Self {
            control,
            interval: Duration::from_millis(100),
        }
    }
    pub(crate) fn run(
        &self,
        process: &mut dyn ProcessHandle,
        output: &mut dyn ProcessOutput,
        mut on_line: impl FnMut(String),
        mut on_warning: impl FnMut(String),
    ) -> Result<ProcessOutcome> {
        let mut output_available = true;
        loop {
            let status = match process.poll() {
                Ok(status) => status,
                Err(error) => {
                    // Do not delegate fallible cleanup to Drop, which cannot report
                    // a still-running installer to the caller / recovery journal.
                    return match process.terminate() {
                        Ok(()) => Err(error).context("进程状态读取失败，已完成进程清理"),
                        Err(cleanup) => Err(error).context(format!(
                            "进程状态读取失败，终止 / 回收也失败，进程可能仍在运行：{cleanup:#}"
                        )),
                    };
                }
            };
            let cancelled = status.is_none() && self.control.cancelled();
            if cancelled {
                process
                    .terminate()
                    .context("取消请求未完成：进程清理失败，进程可能仍在运行")?;
            }
            if output_available {
                match output.read(status.is_some() || cancelled) {
                    Ok(lines) => lines.into_iter().for_each(&mut on_line),
                    Err(error) => {
                        output_available = false;
                        on_warning(format!(
                            "实时日志读取失败，停止跟随日志；继续监控进程：{error:#}"
                        ));
                    }
                }
            }
            if let Some(status) = status {
                return Ok(ProcessOutcome::Exited(status));
            }
            if cancelled {
                return Ok(ProcessOutcome::Cancelled);
            }
            thread::sleep(self.interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    struct Process {
        statuses: VecDeque<Result<Option<ExitStatus>>>,
        terminated: usize,
        fail_terminate: bool,
    }
    impl ProcessHandle for Process {
        fn poll(&mut self) -> Result<Option<ExitStatus>> {
            self.statuses.pop_front().expect("unexpected extra poll")
        }
        fn terminate(&mut self) -> Result<()> {
            self.terminated += 1;
            if self.fail_terminate {
                bail!("stop failed");
            }
            Ok(())
        }
    }
    #[derive(Default)]
    struct Output {
        calls: Vec<bool>,
        fail: bool,
    }
    impl ProcessOutput for Output {
        fn read(&mut self, finished: bool) -> Result<Vec<String>> {
            self.calls.push(finished);
            if self.fail {
                bail!("log unavailable");
            }
            Ok(vec![if finished { "last line" } else { "running" }.into()])
        }
    }
    fn process(statuses: Vec<Result<Option<ExitStatus>>>) -> Process {
        Process {
            statuses: statuses.into(),
            terminated: 0,
            fail_terminate: false,
        }
    }
    fn monitor(control: &TaskControl) -> ProcessMonitor<'_> {
        ProcessMonitor {
            control,
            interval: Duration::ZERO,
        }
    }
    #[test]
    fn broken_log_does_not_terminate_process_or_hide_failed_exit() {
        let control = TaskControl::default();
        let mut child = process(vec![
            Ok(None),
            Ok(None),
            Ok(Some(ExitStatus::from_raw(256))),
        ]);
        let mut output = Output {
            fail: true,
            ..Output::default()
        };
        let mut warnings = vec![];
        let result = monitor(&control)
            .run(
                &mut child,
                &mut output,
                |_| panic!("no lines"),
                |warning| warnings.push(warning),
            )
            .unwrap();
        assert!(matches!(result, ProcessOutcome::Exited(status) if !status.success()));
        assert_eq!(child.terminated, 0);
        assert_eq!(output.calls, [false]);
        assert_eq!(warnings.len(), 1);
    }
    #[test]
    fn already_exited_process_wins_over_late_cancellation_and_drains_output() {
        let control = TaskControl::default();
        control.cancel();
        let mut child = process(vec![Ok(Some(ExitStatus::from_raw(0)))]);
        let mut output = Output::default();
        let mut lines = vec![];
        let result = monitor(&control)
            .run(
                &mut child,
                &mut output,
                |line| lines.push(line),
                |_| panic!("no warning"),
            )
            .unwrap();
        assert!(matches!(result, ProcessOutcome::Exited(status) if status.success()));
        assert_eq!(child.terminated, 0);
        assert_eq!(output.calls, [true]);
        assert_eq!(lines, ["last line"]);
    }
    #[test]
    fn cancellation_terminates_before_draining_output_even_when_paused() {
        let control = TaskControl::default();
        control.toggle_pause();
        control.cancel();
        let mut child = process(vec![Ok(None)]);
        let mut output = Output::default();
        let result = monitor(&control)
            .run(&mut child, &mut output, |_| {}, |_| {})
            .unwrap();
        assert!(matches!(result, ProcessOutcome::Cancelled));
        assert_eq!(child.terminated, 1);
        assert_eq!(output.calls, [true]);
    }
    #[test]
    fn process_poll_and_termination_errors_are_not_reported_as_success() {
        let control = TaskControl::default();
        let mut child = process(vec![Err(anyhow::anyhow!("poll failed"))]);
        let mut output = Output::default();
        let error = monitor(&control)
            .run(&mut child, &mut output, |_| {}, |_| {})
            .err()
            .expect("poll failure must propagate");
        assert!(format!("{error:#}").contains("poll failed"));
        assert_eq!(child.terminated, 1);
        assert!(output.calls.is_empty());
        control.cancel();
        let mut child = process(vec![Ok(None)]);
        child.fail_terminate = true;
        let error = monitor(&control)
            .run(&mut child, &mut output, |_| {}, |_| {})
            .err()
            .expect("cancellation cleanup failure must propagate");
        let message = format!("{error:#}");
        assert!(message.contains("取消请求未完成"));
        assert!(message.contains("stop failed"));
        assert_eq!(child.terminated, 1);
        assert!(output.calls.is_empty());
    }

    #[test]
    fn poll_failure_reports_cleanup_failure_without_claiming_process_stopped() {
        let control = TaskControl::default();
        let mut child = process(vec![Err(anyhow::anyhow!("poll failed"))]);
        child.fail_terminate = true;
        let mut output = Output::default();
        let error = monitor(&control)
            .run(&mut child, &mut output, |_| {}, |_| {})
            .err()
            .expect("both failures must propagate");
        let message = format!("{error:#}");
        assert!(message.contains("poll failed"));
        assert!(message.contains("stop failed"));
        assert!(message.contains("进程可能仍在运行"));
        assert_eq!(child.terminated, 1);
        assert!(output.calls.is_empty());
    }

    fn spawn_probe(
        workspace: &crate::test_support::Workspace,
        mode: &str,
    ) -> (ManagedProcess, LogTail) {
        let path = workspace.path.join("process.log");
        let output = std::fs::File::create(&path).unwrap();
        let tail = LogTail::open(&path).unwrap();
        let child = ManagedProcess::spawn(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "process_service::tests::child_process_probe",
                    "--nocapture",
                ])
                .env("TUI_PROCESS_PROBE_MODE", mode)
                .env("TUI_PROCESS_PROBE_READY", workspace.path.join("ready"))
                .stdout(output.try_clone().unwrap())
                .stderr(output),
        )
        .unwrap();
        (child, tail)
    }
    #[test]
    fn guarded_process_releases_leases_on_spawn_failure_and_confirmed_exit() {
        let workspace = crate::test_support::Workspace::new();
        let destination = workspace.path.join("options.txt");
        let control = TaskControl::default();
        let natives = crate::native_service::NativeDirectory::create(
            &workspace.path.join("natives"),
            &control,
        )
        .unwrap();
        let failed_path = natives.path().unwrap().to_path_buf();
        let lease = crate::download_lock::DownloadLease::try_acquire(&destination).unwrap();
        assert!(ManagedProcess::spawn_guarded(
            &mut Command::new(workspace.path.join("nonexistent-executable")),
            (lease, natives),
        )
        .is_err());
        assert!(!failed_path.exists());
        let natives = crate::native_service::NativeDirectory::create(
            &workspace.path.join("natives"),
            &control,
        )
        .unwrap();
        let native_path = natives.path().unwrap().to_path_buf();
        let lease = crate::download_lock::DownloadLease::try_acquire(&destination).unwrap();
        let (mut child, mut output) = spawn_probe(&workspace, "exit");
        child.resources = Some(Box::new((lease, natives)));
        assert!(native_path.is_dir());
        assert!(crate::download_lock::DownloadLease::try_acquire(&destination).is_err());
        ProcessMonitor::new(&TaskControl::default())
            .run(&mut child, &mut output, |_| {}, |_| {})
            .unwrap();
        drop(child);
        assert!(!native_path.exists());
        let _available = crate::download_lock::DownloadLease::try_acquire(&destination).unwrap();
    }
    #[test]
    fn installer_process_keeps_shared_lease_after_caller_releases_its_copy() {
        use crate::installation_service::LoaderInstallSession;
        use std::sync::Arc;
        let workspace = crate::test_support::Workspace::new();
        let lease = Arc::new(LoaderInstallSession::for_recovery(&workspace.path).unwrap());
        let (mut child, mut output) = spawn_probe(&workspace, "exit");
        child.resources = Some(Box::new(Arc::clone(&lease)));
        drop(lease);
        assert!(LoaderInstallSession::for_recovery(&workspace.path).is_err());
        ProcessMonitor::new(&TaskControl::default())
            .run(&mut child, &mut output, |_| {}, |_| {})
            .unwrap();
        drop(child);
        let _recovery = LoaderInstallSession::for_recovery(&workspace.path).unwrap();
    }
    #[test]
    fn actual_child_failed_exit_preserves_unterminated_output() {
        let workspace = crate::test_support::Workspace::new();
        let (mut child, mut tail) = spawn_probe(&workspace, "exit");
        let control = TaskControl::default();
        let mut lines = vec![];
        let outcome = ProcessMonitor::new(&control)
            .run(
                &mut child,
                &mut tail,
                |line| lines.push(line),
                |_| panic!("unexpected warning"),
            )
            .unwrap();
        assert!(matches!(outcome, ProcessOutcome::Exited(status) if status.code() == Some(17)));
        assert!(lines.iter().any(|line| line.ends_with("进程末行")));
        assert!(child.try_wait().unwrap().is_some());
    }
    #[test]
    fn actual_child_is_reaped_on_cancellation() {
        let workspace = crate::test_support::Workspace::new();
        let (mut child, mut tail) = spawn_probe(&workspace, "wait");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !workspace.path.join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "helper exited before cancellation"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "helper failed to become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let control = TaskControl::default();
        control.cancel();
        let mut lines = vec![];
        let outcome = ProcessMonitor::new(&control)
            .run(
                &mut child,
                &mut tail,
                |line| lines.push(line),
                |_| panic!("unexpected warning"),
            )
            .unwrap();
        assert!(matches!(outcome, ProcessOutcome::Cancelled));
        assert!(child.try_wait().unwrap().is_some());
        assert!(lines.iter().any(|line| line.ends_with("进程末行")));
    }
    #[test]
    fn descendants_stop_on_cancellation_parent_exit_and_owner_drop() {
        for mode in ["tree-wait", "tree-exit", "tree-drop"] {
            let workspace = crate::test_support::Workspace::new();
            let (mut child, mut tail) = spawn_probe(&workspace, mode);
            let ready = workspace.path.join("ready");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "descendant not ready: {mode}"
                );
                thread::sleep(Duration::from_millis(10));
            }
            if mode == "tree-drop" {
                drop(child);
            } else {
                let control = TaskControl::default();
                if mode == "tree-wait" {
                    control.cancel();
                }
                let outcome = ProcessMonitor::new(&control)
                    .run(&mut child, &mut tail, |_| {}, |_| {})
                    .unwrap();
                if mode == "tree-wait" {
                    assert!(matches!(outcome, ProcessOutcome::Cancelled));
                } else {
                    assert!(
                        matches!(outcome, ProcessOutcome::Exited(status) if status.code() == Some(17))
                    );
                }
                assert!(
                    child.tree.is_none(),
                    "containment must confirm empty before returning"
                );
            }
            let heartbeat = workspace.path.join("heartbeat");
            let stopped = std::fs::read(&heartbeat).unwrap();
            thread::sleep(Duration::from_millis(70));
            assert_eq!(
                std::fs::read(heartbeat).unwrap(),
                stopped,
                "descendant kept writing: {mode}"
            );
        }
    }

    #[test]
    #[ignore = "subprocess helper invoked by process monitor tests"]
    fn child_process_probe() {
        let Ok(mode) = std::env::var("TUI_PROCESS_PROBE_MODE") else {
            return;
        };
        let ready = std::path::PathBuf::from(std::env::var_os("TUI_PROCESS_PROBE_READY").unwrap());
        if mode == "leaf" {
            let mut heartbeat = std::fs::File::create(ready.with_file_name("heartbeat")).unwrap();
            heartbeat.write_all(b"ready").unwrap();
            heartbeat.flush().unwrap();
            std::fs::write(&ready, b"ready").unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                heartbeat.write_all(b".").unwrap();
                heartbeat.flush().unwrap();
                thread::sleep(Duration::from_millis(10));
            }
            return;
        }
        if mode.starts_with("tree-") {
            let mut leaf = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "process_service::tests::child_process_probe",
                    "--nocapture",
                ])
                .env("TUI_PROCESS_PROBE_MODE", "leaf")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            if mode == "tree-exit" {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !ready.exists() {
                    assert!(std::time::Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(10));
                }
                std::process::exit(17);
            }
            let _ = leaf.wait();
            return;
        }
        use std::io::Write;
        let mut output = std::io::stdout().lock();
        output.write_all("进程末行".as_bytes()).unwrap();
        output.flush().unwrap();
        drop(output);
        if mode == "wait" {
            std::fs::write(
                std::env::var_os("TUI_PROCESS_PROBE_READY").unwrap(),
                b"ready",
            )
            .unwrap();
            thread::sleep(Duration::from_secs(30));
        }
        std::process::exit(17);
    }
}
