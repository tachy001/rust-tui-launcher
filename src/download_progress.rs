use crate::backend::Message;
use std::{
    sync::{mpsc::Sender, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Snapshot {
    pub label: String,
    pub done: u64,
    pub total: Option<u64>,
    pub speed: f64,
    pub eta: Option<u64>,
}
impl Snapshot {
    pub fn display(&self, paused: bool) -> String {
        let amount = match self.total {
            Some(total) => format!("{} / {}", bytes(self.done), bytes(total)),
            None => format!("{} / 大小未知", bytes(self.done)),
        };
        if paused {
            return format!("{} · {amount} · 已暂停", self.label);
        }
        let eta = self
            .eta
            .map(|seconds| {
                if seconds >= 3600 {
                    format!(" · 约 {}h {}m", seconds / 3600, seconds % 3600 / 60)
                } else if seconds >= 60 {
                    format!(" · 约 {}m {}s", seconds / 60, seconds % 60)
                } else {
                    format!(" · 约 {seconds}s")
                }
            })
            .unwrap_or_default();
        format!(
            "{} · {amount} · {}/s{eta}",
            self.label,
            bytes(self.speed.max(0.0) as u64)
        )
    }
}
fn bytes(value: u64) -> String {
    let (unit, divisor) = if value >= 1024 * 1024 * 1024 {
        ("GiB", 1024.0 * 1024.0 * 1024.0)
    } else if value >= 1024 * 1024 {
        ("MiB", 1024.0 * 1024.0)
    } else if value >= 1024 {
        ("KiB", 1024.0)
    } else {
        ("B", 1.0)
    };
    format!("{:.1} {unit}", value as f64 / divisor)
}
struct State {
    jobs: Vec<(u64, Option<u64>)>,
    received: u64,
    last_received: u64,
    last_report: Instant,
}
pub struct Meter<'a> {
    label: String,
    tx: &'a Sender<Message>,
    state: Mutex<State>,
}
impl<'a> Meter<'a> {
    pub fn new(
        label: &str,
        sizes: impl IntoIterator<Item = Option<u64>>,
        tx: &'a Sender<Message>,
    ) -> Self {
        Self {
            label: label.into(),
            tx,
            state: Mutex::new(State {
                jobs: sizes.into_iter().map(|size| (0, size)).collect(),
                received: 0,
                last_received: 0,
                last_report: Instant::now(),
            }),
        }
    }
    pub fn update(&self, job: usize, position: u64, total: Option<u64>, received: u64) {
        let mut state = self.state.lock().unwrap();
        state.jobs[job].0 = position;
        if total.is_some() {
            state.jobs[job].1 = total;
        }
        state.received = state.received.saturating_add(received);
        if state.last_report.elapsed() >= Duration::from_millis(300) {
            self.send(&mut state);
        }
    }
    pub fn finish(&self) {
        self.send(&mut self.state.lock().unwrap());
    }
    fn send(&self, state: &mut State) {
        let elapsed = state.last_report.elapsed().as_secs_f64().max(0.001);
        let speed = state.received.saturating_sub(state.last_received) as f64 / elapsed;
        let done = state
            .jobs
            .iter()
            .fold(0u64, |sum, (position, _)| sum.saturating_add(*position));
        let total = state.jobs.iter().try_fold(0u64, |sum, (_, size)| {
            size.and_then(|size| sum.checked_add(size))
        });
        let eta = total
            .filter(|total| *total > done)
            .filter(|_| speed >= 1.0)
            .map(|total| ((total - done) as f64 / speed).ceil() as u64);
        let _ = self.tx.send(Message::Transfer(Snapshot {
            label: self.label.clone(),
            done,
            total,
            speed,
            eta,
        }));
        state.last_report = Instant::now();
        state.last_received = state.received;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_and_cache_do_not_inflate_network_rate() {
        let (tx, rx) = std::sync::mpsc::channel();
        let meter = Meter::new("资源", [Some(100), Some(200)], &tx);
        meter.update(0, 80, Some(100), 0);
        meter.update(1, 200, Some(200), 0);
        meter.finish();
        let Message::Transfer(snapshot) = rx.recv().unwrap() else {
            panic!()
        };
        assert_eq!(snapshot.done, 280);
        assert_eq!(snapshot.total, Some(300));
        assert_eq!(snapshot.speed, 0.0);
        assert_eq!(snapshot.eta, None);
        // A restarted response resets its file position, without double-counting.
        meter.update(0, 20, Some(100), 20);
        meter.finish();
        let Message::Transfer(snapshot) = rx.recv().unwrap() else {
            panic!()
        };
        assert_eq!(snapshot.done, 220);
        assert!(snapshot.speed > 0.0);
    }
    #[test]
    fn unknown_sizes_and_pause_do_not_invent_an_eta() {
        let (tx, rx) = std::sync::mpsc::channel();
        let meter = Meter::new("文件", [None], &tx);
        meter.update(0, 1024, None, 1024);
        meter.finish();
        let Message::Transfer(snapshot) = rx.recv().unwrap() else {
            panic!()
        };
        assert!(snapshot.total.is_none());
        assert!(snapshot.eta.is_none());
        assert!(snapshot.display(true).contains("已暂停"));
        assert!(!snapshot.display(true).contains("/s"));
    }
}
