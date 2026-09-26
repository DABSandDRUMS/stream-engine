//! Queue entries and their play order (pending approvals, upcoming, current).

use se_proto::{Role, Value};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting for mod approval.
    Pending,
    Queued,
    Playing,
    Played,
    Skipped,
    Removed,
    Rejected,
    /// The player couldn't play it (embedding disabled, removed, …).
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Queued => "queued",
            Status::Playing => "playing",
            Status::Played => "played",
            Status::Skipped => "skipped",
            Status::Removed => "removed",
            Status::Rejected => "rejected",
            Status::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "pending" => Status::Pending,
            "queued" => Status::Queued,
            "playing" => Status::Playing,
            "played" => Status::Played,
            "skipped" => Status::Skipped,
            "removed" => Status::Removed,
            "rejected" => Status::Rejected,
            "error" => Status::Error,
            _ => return None,
        })
    }
}

/// Payment attached to a request (bits cheer or channel-point redemption).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Paid {
    pub bits: u32,
    pub points: u32,
    pub redemption_id: Option<String>,
    pub reward_id: Option<String>,
    /// Inserted ahead of unpaid requests (policy `paid_skip_line` at request time).
    pub skip_line: bool,
    /// The redemption was fulfilled (it played); never refund after this.
    pub fulfilled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub video: String,
    pub title: String,
    pub channel: String,
    pub duration_s: u32,
    pub user: String,
    pub user_id: Option<String>,
    pub role: Role,
    pub status: Status,
    pub paid: Option<Paid>,
    /// Unix ms.
    pub requested_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub note: Option<String>,
}

pub fn role_str(r: Role) -> &'static str {
    match r {
        Role::Everyone => "everyone",
        Role::Follower => "follower",
        Role::Sub => "sub",
        Role::Vip => "vip",
        Role::Mod => "mod",
        Role::Owner => "owner",
    }
}

impl Entry {
    pub fn skips_line(&self) -> bool {
        self.paid.as_ref().is_some_and(|p| p.skip_line)
    }

    pub fn to_value(&self, pos: Option<usize>) -> Value {
        let mut v = Value::map()
            .with("id", self.id)
            .with("video", self.video.clone())
            .with("title", self.title.clone())
            .with("channel", self.channel.clone())
            .with("duration", self.duration_s as f64)
            .with("user", self.user.clone())
            .with("role", role_str(self.role))
            .with("paid", self.paid.is_some())
            .with("requested_at", self.requested_at)
            .with("status", self.status.as_str())
            .with("url", crate::youtube::watch_url(&self.video));
        if let Some(p) = pos {
            v = v.with("pos", p as i64);
        }
        if let Some(t) = self.started_at {
            v = v.with("started_at", t);
        }
        if let Some(t) = self.ended_at {
            v = v.with("ended_at", t);
        }
        if let Some(n) = &self.note {
            v = v.with("note", n.clone());
        }
        v
    }
}

/// In-memory order. `upcoming[0]` plays next; `pending` is oldest first.
#[derive(Clone, Debug, Default)]
pub struct Queue {
    pub current: Option<Entry>,
    pub upcoming: Vec<Entry>,
    pub pending: Vec<Entry>,
}

impl Queue {
    /// Insert into `upcoming`: skip-the-line entries go after existing skip-the-line entries but
    /// ahead of everything else. Returns the 1-based position.
    pub fn enqueue(&mut self, e: Entry) -> usize {
        let at = if e.skips_line() { self.upcoming.iter().take_while(|x| x.skips_line()).count() } else { self.upcoming.len() };
        self.upcoming.insert(at, e);
        at + 1
    }

    /// 1-based position in `upcoming`.
    pub fn position(&self, id: i64) -> Option<usize> {
        self.upcoming.iter().position(|e| e.id == id).map(|i| i + 1)
    }

    /// Remove from upcoming or pending.
    pub fn take(&mut self, id: i64) -> Option<Entry> {
        if let Some(i) = self.upcoming.iter().position(|e| e.id == id) {
            return Some(self.upcoming.remove(i));
        }
        self.pending.iter().position(|e| e.id == id).map(|i| self.pending.remove(i))
    }

    pub fn take_pending(&mut self, id: i64) -> Option<Entry> {
        self.pending.iter().position(|e| e.id == id).map(|i| self.pending.remove(i))
    }

    /// Move an upcoming entry to 1-based position `to` (clamped). Returns false if unknown.
    pub fn reorder(&mut self, id: i64, to: usize) -> bool {
        let Some(i) = self.upcoming.iter().position(|e| e.id == id) else { return false };
        let e = self.upcoming.remove(i);
        let to = to.clamp(1, self.upcoming.len() + 1) - 1;
        self.upcoming.insert(to, e);
        true
    }

    pub fn pop_next(&mut self) -> Option<Entry> {
        (!self.upcoming.is_empty()).then(|| self.upcoming.remove(0))
    }

    /// Requests by this user that haven't played yet (pending + upcoming).
    pub fn open_requests(&self, login: &str) -> usize {
        self.upcoming.iter().chain(self.pending.iter()).filter(|e| e.user.eq_ignore_ascii_case(login)).count()
    }

    /// Where a video already sits: 0 = playing now, n = upcoming position, None = not queued.
    /// Pending entries count as queued (position = upcoming length + pending index + 1).
    pub fn find_video(&self, video: &str) -> Option<usize> {
        if self.current.as_ref().is_some_and(|c| c.video == video) {
            return Some(0);
        }
        if let Some(p) = self.upcoming.iter().position(|e| e.video == video) {
            return Some(p + 1);
        }
        self.pending.iter().position(|e| e.video == video).map(|p| self.upcoming.len() + p + 1)
    }

    /// Total length counted against `max_queue` (upcoming + pending).
    pub fn len(&self) -> usize {
        self.upcoming.len() + self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most recent open request of a user (for `!wrongsong`).
    pub fn last_of(&self, login: &str) -> Option<i64> {
        self.upcoming.iter().chain(self.pending.iter()).filter(|e| e.user.eq_ignore_ascii_case(login)).max_by_key(|e| e.requested_at).map(|e| e.id)
    }

    pub fn ids_of(&self, login: &str) -> Vec<i64> {
        self.upcoming.iter().chain(self.pending.iter()).filter(|e| e.user.eq_ignore_ascii_case(login)).map(|e| e.id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn entry(id: i64, user: &str, paid: bool) -> Entry {
        Entry {
            id,
            video: format!("vid{id:08}"),
            title: format!("Song {id}"),
            channel: "Ch".into(),
            duration_s: 180,
            user: user.into(),
            user_id: None,
            role: Role::Everyone,
            status: Status::Queued,
            paid: paid.then(|| Paid { points: 500, skip_line: true, ..Default::default() }),
            requested_at: id * 1000,
            started_at: None,
            ended_at: None,
            note: None,
        }
    }

    #[test]
    fn paid_requests_skip_the_line_in_arrival_order() {
        let mut q = Queue::default();
        assert_eq!(q.enqueue(entry(1, "a", false)), 1);
        assert_eq!(q.enqueue(entry(2, "b", false)), 2);
        assert_eq!(q.enqueue(entry(3, "c", true)), 1);
        assert_eq!(q.enqueue(entry(4, "d", true)), 2);
        assert_eq!(q.enqueue(entry(5, "e", false)), 5);
        let order: Vec<i64> = q.upcoming.iter().map(|e| e.id).collect();
        assert_eq!(order, vec![3, 4, 1, 2, 5]);
    }

    #[test]
    fn reorder_clamps_and_reports_unknown() {
        let mut q = Queue::default();
        for i in 1..=4 {
            q.enqueue(entry(i, "u", false));
        }
        assert!(q.reorder(4, 1));
        assert!(q.reorder(1, 99));
        assert!(!q.reorder(42, 1));
        let order: Vec<i64> = q.upcoming.iter().map(|e| e.id).collect();
        assert_eq!(order, vec![4, 2, 3, 1]);
        assert_eq!(q.position(3), Some(3));
    }

    #[test]
    fn per_user_counts_and_duplicates() {
        let mut q = Queue::default();
        q.enqueue(entry(1, "Alice", false));
        q.pending.push(entry(2, "alice", false));
        q.enqueue(entry(3, "bob", false));
        q.current = Some(entry(9, "carol", false));
        assert_eq!(q.open_requests("ALICE"), 2);
        assert_eq!(q.last_of("alice"), Some(2));
        assert_eq!(q.find_video("vid00000009"), Some(0));
        assert_eq!(q.find_video("vid00000003"), Some(2));
        assert_eq!(q.find_video("vid00000002"), Some(3));
        assert_eq!(q.find_video("nope"), None);
        assert_eq!(q.len(), 3);
        assert_eq!(q.take(2).map(|e| e.id), Some(2));
        assert_eq!(q.pop_next().map(|e| e.id), Some(1));
    }
}
