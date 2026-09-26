//! Causal trace (§2.3, §15.5): event → rule → command → state change chains.

use se_proto::wire::TraceRec;
use se_proto::{Id, Ts};
use std::collections::{HashMap, VecDeque};

pub const CAPACITY: usize = 50_000;

#[derive(Default)]
pub struct Trace {
    recs: VecDeque<TraceRec>,
    children: HashMap<Id, Vec<Id>>,
    by_id: HashMap<Id, usize>,
    base: usize,
    /// Records added since the last drain (pushed to subscribed clients).
    fresh: Vec<TraceRec>,
}

impl Trace {
    pub fn add(&mut self, id: Id, parent: Option<Id>, ts: Ts, kind: &str, label: String) {
        let rec = TraceRec { id, parent, ts, kind: kind.into(), label };
        if self.recs.len() >= CAPACITY
            && let Some(old) = self.recs.pop_front()
        {
            self.by_id.remove(&old.id);
            if let Some(p) = old.parent
                && let Some(c) = self.children.get_mut(&p)
            {
                c.retain(|x| *x != old.id);
                if c.is_empty() {
                    self.children.remove(&p);
                }
            }
            self.children.remove(&old.id);
            self.base += 1;
        }
        if let Some(p) = parent {
            self.children.entry(p).or_default().push(id);
        }
        self.by_id.insert(id, self.base + self.recs.len());
        self.fresh.push(rec.clone());
        self.recs.push_back(rec);
    }

    pub fn get(&self, id: Id) -> Option<&TraceRec> {
        self.by_id.get(&id).and_then(|i| self.recs.get(i - self.base))
    }

    /// Ancestors (root first), the record itself, then all descendants (depth-first).
    pub fn chain(&self, id: Id) -> Vec<TraceRec> {
        let mut up = Vec::new();
        let mut cur = self.get(id).and_then(|r| r.parent);
        let mut guard = 0;
        while let Some(p) = cur {
            match self.get(p) {
                Some(r) => {
                    up.push(r.clone());
                    cur = r.parent;
                }
                None => break,
            }
            guard += 1;
            if guard > 1000 {
                break;
            }
        }
        up.reverse();
        let mut out = up;
        let mut stack = vec![id];
        let mut seen = 0;
        while let Some(x) = stack.pop() {
            if let Some(r) = self.get(x) {
                out.push(r.clone());
            }
            if let Some(c) = self.children.get(&x) {
                stack.extend(c.iter().rev());
            }
            seen += 1;
            if seen > 10_000 {
                break;
            }
        }
        out
    }

    /// Most recent records (newest last).
    pub fn recent(&self, n: usize) -> Vec<TraceRec> {
        self.recs.iter().rev().take(n).rev().cloned().collect()
    }

    pub fn drain_fresh(&mut self) -> Vec<TraceRec> {
        std::mem::take(&mut self.fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_up_and_down() {
        let mut t = Trace::default();
        t.add(1, None, 0, "event", "twitch.cheer".into());
        t.add(2, Some(1), 0, "rule", "cheers".into());
        t.add(3, Some(2), 0, "command", "preset.fire hype".into());
        t.add(4, Some(3), 0, "change", "fx.x = 1".into());
        t.add(5, None, 0, "event", "other".into());
        let c = t.chain(3);
        let ids: Vec<Id> = c.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(t.chain(1).len(), 4);
    }
}
