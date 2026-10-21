use crate::{logs::LogTail, task::TaskControl};
use anyhow::{Context, Result};
use std::{
    process::{Child, ExitStatus},
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
pub(crate) struct ManagedProcess(pub(crate) Child);
impl std::ops::Deref for ManagedProcess {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ManagedProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl ProcessHandle for ManagedProcess {
    fn poll(&mut self) -> Result<Option<ExitStatus>> {
        self.0.try_wait().context("读取进程退出状态失败")
    }
    fn terminate(&mut self) -> Result<()> {
        if self.0.try_wait()?.is_none() {
            if let Err(error) = self.0.kill() {
                // The process may have exited between the status check and kill.
                if self.0.try_wait()?.is_none() {
                    return Err(error).context("终止进程失败");
                }
            }
        }
        self.0.wait().context("等待进程退出失败")?;
        Ok(())
    }
}
impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.terminate();
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
            let status = process.poll()?;
            let cancelled = status.is_none() && self.control.cancelled();
            if cancelled {
                process.terminate()?;
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
        assert!(monitor(&control)
            .run(&mut child, &mut output, |_| {}, |_| {})
            .is_err());
        assert!(output.calls.is_empty());
        control.cancel();
        let mut child = process(vec![Ok(None)]);
        child.fail_terminate = true;
        assert!(monitor(&control)
            .run(&mut child, &mut output, |_| {}, |_| {})
            .is_err());
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
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "process_service::tests::child_process_probe",
                "--nocapture",
            ])
            .env("TUI_PROCESS_PROBE_MODE", mode)
            .env("TUI_PROCESS_PROBE_READY", workspace.path.join("ready"))
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        (ManagedProcess(child), tail)
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
    #[ignore = "subprocess helper invoked by process monitor tests"]
    fn child_process_probe() {
        let Ok(mode) = std::env::var("TUI_PROCESS_PROBE_MODE") else {
            return;
        };
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
