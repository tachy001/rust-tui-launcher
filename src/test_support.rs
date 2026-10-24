use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static SERIAL: AtomicU64 = AtomicU64::new(0);

/// Bound connection waits and normalize accepted sockets on every platform.
pub fn accept_http(listener: &std::net::TcpListener) -> std::io::Result<std::net::TcpStream> {
    use std::{
        io,
        time::{Duration, Instant},
    };
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Windows can inherit the listener's nonblocking mode.
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                return Ok(stream);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "HTTP fixture received no connection",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}
pub struct Workspace {
    pub path: PathBuf,
}
impl Workspace {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tui-mc-test-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if let (Ok(root), Ok(temporary)) = (
            fs::canonicalize(&self.path),
            fs::canonicalize(std::env::temp_dir()),
        ) {
            if root.starts_with(&temporary)
                && root != temporary
                && root
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("tui-mc-test-"))
            {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
}
