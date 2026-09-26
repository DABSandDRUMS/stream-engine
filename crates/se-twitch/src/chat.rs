//! Outgoing chat: Twitch's rolling 30-second message limit and the 500-character cap.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const MAX_CHARS: usize = 500;
pub const WINDOW: Duration = Duration::from_secs(30);

/// Rolling-window limiter: at most `limit` sends in any [`WINDOW`].
pub struct RateWindow {
    sent: VecDeque<Instant>,
    limit: usize,
}

impl RateWindow {
    pub fn new(limit: u32) -> RateWindow {
        RateWindow { sent: VecDeque::new(), limit: limit.max(1) as usize }
    }

    pub fn set_limit(&mut self, limit: u32) {
        self.limit = limit.max(1) as usize;
    }

    /// How long to wait before the next send is allowed (`None` = now).
    pub fn wait(&mut self, now: Instant) -> Option<Duration> {
        while self.sent.front().is_some_and(|t| now.duration_since(*t) >= WINDOW) {
            self.sent.pop_front();
        }
        if self.sent.len() < self.limit { None } else { self.sent.front().map(|t| WINDOW - now.duration_since(*t)) }
    }

    pub fn record(&mut self, now: Instant) {
        self.sent.push_back(now);
    }
}

/// Chat text as sent: control characters removed, cut to 500 characters.
pub fn cap(text: &str) -> String {
    let t = se_core::policy::display_text(text, MAX_CHARS, 3);
    if t.chars().count() > MAX_CHARS { t.chars().take(MAX_CHARS - 1).chain(std::iter::once('…')).collect() } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_window() {
        let t0 = Instant::now();
        let mut r = RateWindow::new(2);
        assert_eq!(r.wait(t0), None);
        r.record(t0);
        r.record(t0 + Duration::from_secs(1));
        assert_eq!(r.wait(t0 + Duration::from_secs(10)), Some(Duration::from_secs(20)));
        assert_eq!(r.wait(t0 + Duration::from_secs(30)), None, "oldest send left the window");
    }

    #[test]
    fn capped_to_twitch_limit() {
        let long = "a".repeat(800);
        assert_eq!(cap(&long).chars().count(), MAX_CHARS);
        assert_eq!(cap("hi\u{0}\nthere"), "hi there");
    }
}
