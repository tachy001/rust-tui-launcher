use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static SERIAL: AtomicU64 = AtomicU64::new(0);
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
