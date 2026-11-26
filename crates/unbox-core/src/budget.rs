use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Each extraction phase reserves its own write budget before touching disk, so a
/// misdeclared size can never fill the disk.
#[derive(Debug)]
pub struct OutputBudget {
    limit: u64,
    reserved: AtomicU64,
    written: AtomicU64,
    exceeded: AtomicBool,
}

impl OutputBudget {
    pub fn new(limit: u64) -> Self {
        Self { limit, reserved: AtomicU64::new(0), written: AtomicU64::new(0), exceeded: AtomicBool::new(false) }
    }

    /// Reserve `n` bytes. Returns `false` (and latches the exceeded flag) when the
    /// reservation does not fit.
    pub fn consume(&self, n: u64) -> bool {
        let mut reserved = self.reserved.load(Ordering::Acquire);
        loop {
            if n > self.limit - reserved {
                self.exceeded.store(true, Ordering::Release);
                return false;
            }
            match self.reserved.compare_exchange_weak(reserved, reserved + n, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(current) => reserved = current,
            }
        }
    }

    /// Convert a reservation into the number of bytes actually written.
    pub fn settle(&self, reserved: u64, written: u64) {
        self.written.fetch_add(written, Ordering::AcqRel);
        self.reserved.fetch_sub(reserved - written, Ordering::AcqRel);
    }

    pub fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::Acquire)
    }

    pub fn used(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }
}

/// Default budget: the declared size plus a quarter (at least 64 MiB) of slack.
pub fn default_budget(known_bytes: u64, minimum: u64) -> u64 {
    let slack = (known_bytes / 4).max(64 * 1024 * 1024);
    minimum.max(known_bytes.saturating_add(slack))
}

/// Parse a positive decimal byte count.
pub fn parse_budget(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().filter(|v| *v != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_latches_when_exceeded() {
        let budget = OutputBudget::new(10);
        assert!(budget.consume(6));
        budget.settle(6, 6);
        assert!(!budget.consume(5));
        assert!(budget.exceeded());
        assert_eq!(budget.used(), 6);
    }

    #[test]
    fn default_budget_has_slack() {
        assert_eq!(default_budget(0, 1), 64 * 1024 * 1024);
        assert_eq!(default_budget(400 << 20, 0), 500 << 20);
        assert_eq!(default_budget(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn parse_budget_rejects_garbage() {
        assert_eq!(parse_budget("123"), Some(123));
        assert_eq!(parse_budget("0"), None);
        assert_eq!(parse_budget("-1"), None);
        assert_eq!(parse_budget("99999999999999999999999"), None);
    }
}
