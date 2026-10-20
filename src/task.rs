use anyhow::{bail, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{thread, time::Duration};

#[derive(Clone, Default)]
pub struct TaskControl {
    cancelled: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}
impl TaskControl {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    pub fn toggle_pause(&self) -> bool {
        let old = self.paused.fetch_xor(true, Ordering::Relaxed);
        !old
    }
    pub fn check(&self) -> Result<()> {
        loop {
            if self.cancelled() {
                bail!("任务已取消；已下载的部分文件会保留以便继续");
            }
            if !self.paused.load(Ordering::Relaxed) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelling_wakes_paused_task() {
        let control = TaskControl::default();
        control.toggle_pause();
        let worker = control.clone();
        let handle = thread::spawn(move || worker.check());
        control.cancel();
        assert!(handle.join().unwrap().is_err());
    }
}
