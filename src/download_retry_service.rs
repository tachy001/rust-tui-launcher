use crate::task::TaskControl;
use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

const MAX_AUTO_WAIT: Duration = Duration::from_secs(60);
#[derive(Clone, Copy)]
struct Cooldown {
    until: Option<Instant>,
    requested: Duration,
}
#[derive(Default)]
pub(crate) struct DownloadRetryCoordinator {
    deadlines: Mutex<HashMap<String, Cooldown>>,
}
impl DownloadRetryCoordinator {
    pub(crate) fn remaining(&self, url: &str) -> Duration {
        self.remaining_at(url, Instant::now())
            .unwrap_or(Duration::MAX)
    }
    fn remaining_at(&self, url: &str, now: Instant) -> Result<Duration> {
        let origin = origin(url)?;
        let deadlines = self
            .deadlines
            .lock()
            .map_err(|_| anyhow::anyhow!("下载源等待状态不可用"))?;
        Ok(deadlines
            .get(&origin)
            .map(|deadline| match deadline.until {
                Some(until) => until.saturating_duration_since(now),
                None => deadline.requested,
            })
            .unwrap_or_default())
    }
    pub(crate) fn response(
        &self,
        url: &str,
        status: u16,
        retry_after: Option<&str>,
        server_date: Option<&str>,
    ) -> Result<()> {
        if !matches!(status, 429 | 503) {
            return Ok(());
        }
        let now = server_date
            .and_then(|date| httpdate::parse_http_date(date).ok())
            .unwrap_or_else(SystemTime::now);
        let delay = retry_after
            .and_then(|value| retry_delay(value, now))
            .unwrap_or_else(|| Duration::from_secs(if status == 429 { 10 } else { 1 }));
        self.defer_at(url, delay, Instant::now())
    }
    fn defer_at(&self, url: &str, delay: Duration, now: Instant) -> Result<()> {
        let origin = origin(url)?;
        let mut deadlines = self
            .deadlines
            .lock()
            .map_err(|_| anyhow::anyhow!("下载源等待状态不可用"))?;
        deadlines.retain(|_, deadline| deadline.until.is_none_or(|until| until > now));
        let next = Cooldown {
            until: now.checked_add(delay),
            requested: delay,
        };
        let old_remaining = deadlines.get(&origin).map(|old| {
            old.until
                .map(|until| until.saturating_duration_since(now))
                .unwrap_or(old.requested)
        });
        if old_remaining.is_none_or(|old| delay > old) {
            deadlines.insert(origin, next);
        }
        Ok(())
    }
    pub(crate) fn wait(
        &self,
        url: &str,
        control: &TaskControl,
        notify: impl FnOnce(&str, u64),
    ) -> Result<()> {
        control.check()?;
        let remaining = self.remaining_at(url, Instant::now())?;
        if remaining.is_zero() {
            return Ok(());
        }
        let name = origin(url)?;
        if remaining > MAX_AUTO_WAIT {
            bail!(
                "下载源 {name} 要求的等待超过本任务60秒自动等待上限，请稍后重试；不会提前重新请求"
            );
        }
        notify(
            &name,
            remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0),
        );
        let budget_end = Instant::now() + MAX_AUTO_WAIT;
        loop {
            control.check()?;
            let remaining = self.remaining_at(url, Instant::now())?;
            if remaining.is_zero() {
                return Ok(());
            }
            if remaining > budget_end.saturating_duration_since(Instant::now()) {
                bail!("下载源 {name} 延长了等待时间，请稍后重试");
            }
            control.wait(remaining.min(Duration::from_millis(50)))?;
        }
    }
}
fn origin(url: &str) -> Result<String> {
    Ok(reqwest::Url::parse(url)
        .context("下载地址无效")?
        .origin()
        .ascii_serialization())
}
fn retry_delay(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        // An overflowing integer must not turn into a short fallback wait.
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    Some(
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_supports_seconds_dates_and_huge_values_without_early_retry() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10000);
        assert_eq!(retry_delay("12", now), Some(Duration::from_secs(12)));
        assert_eq!(
            retry_delay(&httpdate::fmt_http_date(now + Duration::from_secs(30)), now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            retry_delay(&httpdate::fmt_http_date(now - Duration::from_secs(30)), now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_delay("99999999999999999999999999", now),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(retry_delay("not a date", now), None);
    }
    #[test]
    fn origins_share_cooldown_without_shortening_it_or_delaying_other_hosts() {
        let coordinator = DownloadRetryCoordinator::default();
        let now = Instant::now();
        coordinator
            .defer_at(
                "https://user:private@one.invalid/a?secret",
                Duration::from_secs(20),
                now,
            )
            .unwrap();
        coordinator
            .defer_at("https://one.invalid/b", Duration::from_secs(1), now)
            .unwrap();
        assert_eq!(
            coordinator
                .remaining_at("https://one.invalid/other", now)
                .unwrap(),
            Duration::from_secs(20)
        );
        assert_eq!(
            coordinator
                .remaining_at("https://two.invalid/other", now)
                .unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            coordinator
                .remaining_at("http://one.invalid/other", now)
                .unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            coordinator
                .remaining_at("https://one.invalid:444/other", now)
                .unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            coordinator
                .remaining_at("https://one.invalid/a", now + Duration::from_secs(21))
                .unwrap(),
            Duration::ZERO
        );
        assert!(!format!("{:?}", coordinator.deadlines.lock().unwrap().keys()).contains("private"));
    }
    #[test]
    fn waiting_can_be_cancelled_and_excessive_delay_fails_without_sleeping() {
        let coordinator = std::sync::Arc::new(DownloadRetryCoordinator::default());
        coordinator
            .response("https://one.invalid/a", 429, Some("10"), None)
            .unwrap();
        let control = TaskControl::default();
        let worker_control = control.clone();
        let worker = coordinator.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            worker.wait("https://one.invalid/b", &worker_control, |_, _| {
                tx.send(()).unwrap();
            })
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        control.cancel();
        assert!(handle.join().unwrap().is_err());
        coordinator
            .response("https://two.invalid/a", 503, Some("3600"), None)
            .unwrap();
        assert!(coordinator
            .wait(
                "https://two.invalid/a",
                &TaskControl::default(),
                |_, _| panic!("no long wait")
            )
            .is_err());
    }
}
