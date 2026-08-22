//! Toast/status-line primitive driven by an INJECTABLE tick counter.
//! NO wall-clock anywhere: snapshots stay deterministic.

pub struct Toast {
    pub message: String,
    ticks_left: u32,
}

impl Toast {
    pub fn new(message: impl Into<String>, ttl_ticks: u32) -> Self {
        Self {
            message: message.into(),
            ticks_left: ttl_ticks,
        }
    }

    /// Advance the virtual clock by one tick; returns true while visible.
    pub fn tick(&mut self) -> bool {
        self.ticks_left = self.ticks_left.saturating_sub(1);
        self.ticks_left > 0
    }

    pub fn visible(&self) -> bool {
        self.ticks_left > 0
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_with_positive_ttl_is_visible() {
        let toast = Toast::new("gate passed", 3);
        assert!(toast.visible());
        assert_eq!(toast.message(), "gate passed");
        assert_eq!(toast.message, "gate passed");
    }

    #[test]
    fn tick_expires_after_ttl_ticks() {
        let mut toast = Toast::new("appended entry 7", 3);
        assert!(toast.tick()); // 2 left
        assert!(toast.visible());
        assert!(toast.tick()); // 1 left
        assert!(toast.visible());
        assert!(!toast.tick()); // 0 left
        assert!(!toast.visible());
    }

    #[test]
    fn tick_saturates_at_zero_and_stays_invisible() {
        let mut toast = Toast::new("expired", 1);
        assert!(!toast.tick()); // 0 left
        assert!(!toast.tick()); // saturating_sub keeps 0
        assert!(!toast.tick());
        assert!(!toast.visible());
    }

    #[test]
    fn zero_ttl_starts_invisible_and_stays_saturated() {
        let mut toast = Toast::new("never shown", 0);
        assert!(!toast.visible());
        assert!(!toast.tick());
        assert!(!toast.visible());
    }
}
