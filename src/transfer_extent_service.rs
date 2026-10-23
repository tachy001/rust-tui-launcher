use anyhow::{bail, Context, Result};

/// Tracks the permitted response extent independently of HTTP and filesystem IO.
pub(crate) struct TransferExtent {
    position: u64,
    body_end: Option<u64>,
    total: Option<u64>,
    ranged: bool,
}
impl TransferExtent {
    pub(crate) fn new(
        start: u64,
        body_length: Option<u64>,
        range_total: Option<u64>,
        expected_total: Option<u64>,
        ranged: bool,
    ) -> Result<Self> {
        let body_end = body_length
            .map(|size| start.checked_add(size).context("下载响应长度溢出"))
            .transpose()?;
        if matches!((range_total, expected_total), (Some(a), Some(b)) if a != b) {
            bail!("断点响应总大小与下载元数据不符");
        }
        let total = expected_total.or(range_total).or(body_end);
        if total.is_some_and(|total| start > total || body_end.is_some_and(|end| end > total)) {
            bail!("响应范围超过文件大小");
        }
        if !ranged && matches!((body_end, expected_total), (Some(a), Some(b)) if a != b) {
            bail!("响应长度与下载元数据不符");
        }
        Ok(Self {
            position: start,
            body_end,
            total,
            ranged,
        })
    }
    pub(crate) fn accept(&mut self, bytes: usize) -> Result<u64> {
        let next = self
            .position
            .checked_add(bytes as u64)
            .context("下载字节数溢出")?;
        if self.body_end.is_some_and(|end| next > end)
            || self.total.is_some_and(|total| next > total)
        {
            bail!("响应数据超过声明长度，已拒绝写入多余字节");
        }
        self.position = next;
        Ok(next)
    }
    pub(crate) fn finish(&self) -> Result<()> {
        if self.body_end.is_some_and(|end| self.position != end)
            || (!self.ranged && self.total.is_some_and(|total| self.position != total))
        {
            bail!("响应数据未达到声明长度，保留断点供重试");
        }
        Ok(())
    }
    pub(crate) fn total(&self) -> Option<u64> {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_conflicts_and_overflow_are_rejected_before_transfer() {
        assert!(TransferExtent::new(0, Some(4), None, Some(3), false).is_err());
        assert!(TransferExtent::new(0, Some(2), None, Some(3), false).is_err());
        assert!(TransferExtent::new(1, Some(2), Some(4), Some(3), true).is_err());
        assert!(TransferExtent::new(u64::MAX, Some(1), None, None, true).is_err());
        assert!(TransferExtent::new(4, None, Some(3), None, true).is_err());
    }
    #[test]
    fn known_file_and_range_limits_reject_bytes_without_advancing_position() {
        let mut full = TransferExtent::new(0, None, None, Some(3), false).unwrap();
        assert!(full.accept(4).is_err());
        assert_eq!(full.accept(2).unwrap(), 2);
        assert!(full.finish().is_err());
        assert_eq!(full.accept(1).unwrap(), 3);
        full.finish().unwrap();
        let mut range = TransferExtent::new(1, Some(1), Some(3), Some(3), true).unwrap();
        assert!(range.accept(2).is_err());
        assert_eq!(range.accept(1).unwrap(), 2);
        range.finish().unwrap();
        let empty = TransferExtent::new(0, Some(0), None, Some(0), false).unwrap();
        empty.finish().unwrap();
    }
}
