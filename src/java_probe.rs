use crate::{
    java_architecture::JavaArchitecture,
    java_version::JavaVersion,
    process_service::{ManagedProcess, ProcessHandle},
    task::TaskControl,
};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static SERIAL: AtomicU64 = AtomicU64::new(0);

struct CaptureDirectory {
    path: PathBuf,
    parent: PathBuf,
    cleanup: bool,
}
impl CaptureDirectory {
    fn create(root: &Path) -> Result<Self> {
        let parent = fs::canonicalize(root).context("无法访问 Java 探测临时目录")?;
        let path = parent.join(format!(
            "tui-java-probe-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&path)?;
        Ok(Self {
            path,
            parent,
            cleanup: true,
        })
    }
    fn file(&self, name: &str) -> Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(self.path.join(name))?)
    }
}
impl Drop for CaptureDirectory {
    fn drop(&mut self) {
        if !self.cleanup {
            return;
        }
        if fs::canonicalize(&self.parent).ok().as_ref() != Some(&self.parent) {
            return;
        }
        let Ok(path) = fs::canonicalize(&self.path) else {
            return;
        };
        if path == self.path
            && path.parent() == Some(self.parent.as_path())
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("tui-java-probe-"))
        {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Probes are short lived subprocesses; they do not share the game's unlimited lifetime.
pub(crate) struct JavaProbe<'a> {
    control: &'a TaskControl,
    timeout: Duration,
    output_limit: u64,
    root: PathBuf,
}
impl<'a> JavaProbe<'a> {
    pub(crate) fn new(control: &'a TaskControl) -> Self {
        Self {
            control,
            timeout: Duration::from_secs(15),
            output_limit: 1024 * 1024,
            root: std::env::temp_dir(),
        }
    }
    pub(crate) fn inspect(
        &self,
        executable: &Path,
    ) -> Result<(JavaVersion, JavaArchitecture, String)> {
        self.run(Command::new(executable).args(["-XshowSettings:properties", "-version"]))
    }
    fn run(&self, command: &mut Command) -> Result<(JavaVersion, JavaArchitecture, String)> {
        self.control.check()?;
        crate::java_environment_service::JavaEnvironment.apply(command);
        let mut directory = CaptureDirectory::create(&self.root)?;
        let mut stdout = directory.file("stdout")?;
        let mut stderr = directory.file("stderr")?;
        self.control.check()?;
        let mut child = match ManagedProcess::spawn(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout.try_clone()?))
                .stderr(Stdio::from(stderr.try_clone()?)),
        ) {
            Ok(child) => child,
            Err(error) => {
                if error.is::<crate::process_tree_service::SpawnCleanupUnconfirmed>() {
                    directory.cleanup = false;
                    return Err(error).context(format!(
                        "Java 启动未完成，保留检测输出：{}",
                        directory.path.display()
                    ));
                }
                return Err(error).context("找不到或无法执行 Java，请检查可执行文件路径");
            }
        };
        let result = (|| -> Result<(JavaVersion, JavaArchitecture, String)> {
            let started = Instant::now();
            let status = loop {
                if self.control.cancelled() {
                    bail!("Java 检测已取消");
                }
                let status = child.poll()?;
                let bytes = stdout
                    .metadata()?
                    .len()
                    .checked_add(stderr.metadata()?.len())
                    .context("Java 检测输出长度溢出")?;
                if bytes > self.output_limit {
                    bail!(
                        "Java 检测输出超过限制（{} 字节），已停止检测",
                        self.output_limit
                    );
                }
                if let Some(status) = status {
                    break status;
                }
                if started.elapsed() >= self.timeout {
                    bail!(
                        "Java 检测超时（{} 秒），已停止检测",
                        self.timeout.as_secs_f64()
                    );
                }
                thread::sleep(Duration::from_millis(25));
            };
            let mut out = Vec::new();
            let mut err = Vec::new();
            stdout.seek(SeekFrom::Start(0))?;
            stderr.seek(SeekFrom::Start(0))?;
            (&mut stdout)
                .take(self.output_limit + 1)
                .read_to_end(&mut out)?;
            (&mut stderr)
                .take(self.output_limit + 1)
                .read_to_end(&mut err)?;
            if out.len().saturating_add(err.len()) as u64 > self.output_limit {
                bail!("Java 检测输出超过限制，已停止检测");
            }
            let info = format!(
                "{}\n{}",
                String::from_utf8_lossy(&err),
                String::from_utf8_lossy(&out)
            );
            if !status.success() {
                bail!("Java 检测失败（{status}）：{info}");
            }
            let (version, description) = JavaVersion::from_output(&info)?;
            let architecture = JavaArchitecture::from_output(&info)?;
            Ok((version, architecture, description))
        })();
        // Capture files cannot be removed while a failed / cancelled probe's
        // descendants may still be writing. Unlike Drop, this reports failure.
        let cleanup = child.terminate();
        if cleanup.is_err() {
            directory.cleanup = false;
        }
        match (result, cleanup) {
            (result, Ok(())) => result,
            (Ok(_), Err(cleanup)) => Err(cleanup).context(format!(
                "Java 检测进程清理失败，输出已保留：{}",
                directory.path.display()
            )),
            (Err(error), Err(cleanup)) => Err(error).context(format!(
                "Java 检测失败且进程清理未确认，输出已保留于 {}：{cleanup:#}",
                directory.path.display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn probe<'a>(
        workspace: &crate::test_support::Workspace,
        control: &'a TaskControl,
        timeout: Duration,
        output_limit: u64,
    ) -> JavaProbe<'a> {
        JavaProbe {
            control,
            timeout,
            output_limit,
            root: workspace.path.clone(),
        }
    }
    fn command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "java_probe::tests::child_probe",
                "--nocapture",
            ])
            .env("TUI_JAVA_PROBE_MODE", mode);
        command
    }
    fn assert_clean(workspace: &crate::test_support::Workspace) {
        assert!(fs::read_dir(&workspace.path).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("tui-java-probe-")));
    }
    #[test]
    fn real_helper_probe_reads_stderr_version_and_cleans_capture_files() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let (version, architecture, description) =
            probe(&workspace, &control, Duration::from_secs(5), 65536)
                .run(
                    command("version")
                        .env("JAVA_TOOL_OPTIONS", "invalid-fixture-option")
                        .env("_JAVA_OPTIONS", "invalid-fixture-option")
                        .env("JDK_JAVA_OPTIONS", "invalid-fixture-option")
                        .env("TUI_EXPECT_CLEAN_JAVA_ENV", "yes"),
                )
                .unwrap();
        assert_eq!(version.to_string(), "21.0.7+6-LTS");
        assert_eq!(architecture, JavaArchitecture::Arm64);
        assert!(description.starts_with("openjdk version"));
        assert_clean(&workspace);
    }
    #[test]
    fn timeout_and_excessive_output_terminate_helpers_without_a_jvm() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let started = Instant::now();
        let error = probe(&workspace, &control, Duration::from_millis(150), 65536)
            .run(&mut command("hang"))
            .unwrap_err();
        assert!(error.to_string().contains("超时"));
        assert!(started.elapsed() < Duration::from_secs(5));
        let error = probe(&workspace, &control, Duration::from_secs(5), 4096)
            .run(&mut command("flood"))
            .unwrap_err();
        assert!(error.to_string().contains("输出超过限制"));
        assert_clean(&workspace);
    }
    #[test]
    fn cancellation_stops_a_running_probe_and_failure_status_is_preserved() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        let ready = workspace.path.join("ready");
        let signal = control.clone();
        let waiting = ready.clone();
        let canceller = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !waiting.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            signal.cancel();
        });
        let error = probe(&workspace, &control, Duration::from_secs(5), 65536)
            .run(command("hang").env("TUI_JAVA_PROBE_READY", &ready))
            .unwrap_err();
        canceller.join().unwrap();
        assert!(error.to_string().contains("已取消"));
        let error = probe(
            &workspace,
            &TaskControl::default(),
            Duration::from_secs(5),
            65536,
        )
        .run(&mut command("fail"))
        .unwrap_err();
        assert!(error.to_string().contains("检测失败"));
        assert_clean(&workspace);
    }
    #[test]
    fn already_cancelled_probe_does_not_create_capture_files_or_a_child() {
        let workspace = crate::test_support::Workspace::new();
        let control = TaskControl::default();
        control.cancel();
        let error = probe(&workspace, &control, Duration::from_secs(5), 65536)
            .run(&mut command("hang"))
            .unwrap_err();
        assert!(error.to_string().contains("已取消"));
        assert_eq!(fs::read_dir(&workspace.path).unwrap().count(), 0);
    }
    #[test]
    #[ignore = "subprocess fixture; does not execute Java"]
    fn child_probe() {
        use std::io::Write;
        let Ok(mode) = std::env::var("TUI_JAVA_PROBE_MODE") else {
            return;
        };
        if std::env::var("TUI_EXPECT_CLEAN_JAVA_ENV").as_deref() == Ok("yes") {
            for key in ["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "JDK_JAVA_OPTIONS"] {
                assert!(std::env::var_os(key).is_none());
            }
        }
        match mode.as_str() {
            "version" => eprintln!("Property settings:\n    os.arch = aarch64\n    sun.arch.data.model = 64\nopenjdk version \"21.0.7+6-LTS\" 2025-04-15"),
            "flood" => std::io::stderr().write_all(&vec![b'x'; 8192]).unwrap(),
            "hang" => {
                if let Some(ready) = std::env::var_os("TUI_JAVA_PROBE_READY") {
                    fs::write(ready, b"ready").unwrap();
                }
                thread::sleep(Duration::from_secs(10));
            }
            "fail" => std::process::exit(9),
            _ => panic!("unknown probe mode"),
        }
    }
}
