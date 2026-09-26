//! Beat grid from the offline analysis of library songs (§8.4): snapping for recorded cues
//! and editor moves.

use se_core::timeline::Snap;
use se_proto::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grid {
    pub bpm: f64,
    pub beats: Vec<f64>,
    pub downbeats: Vec<f64>,
}

impl Grid {
    /// From the `analysis.grid` reply (`{bpm, beats, downbeats, …}`); `None` for null/empty.
    pub fn from_value(v: &Value) -> Option<Grid> {
        let list = |k: &str| -> Vec<f64> {
            let mut l: Vec<f64> =
                v.get_path(k).and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_f64).filter(|x| x.is_finite()).collect()).unwrap_or_default();
            l.sort_by(f64::total_cmp);
            l
        };
        let g = Grid { bpm: v.get_path("bpm").and_then(Value::as_f64).unwrap_or(0.0), beats: list("beats"), downbeats: list("downbeats") };
        (!g.beats.is_empty() || !g.downbeats.is_empty()).then_some(g)
    }

    /// Nearest beat (or bar line) to `t`; `t` unchanged when snapping is off or the grid has
    /// no points of that kind.
    pub fn snap(&self, t: f64, mode: Snap) -> f64 {
        let pts = match mode {
            Snap::Off => return t,
            Snap::Beat => &self.beats,
            Snap::Bar => {
                if self.downbeats.is_empty() {
                    &self.beats
                } else {
                    &self.downbeats
                }
            }
        };
        nearest(pts, t).unwrap_or(t)
    }
}

fn nearest(pts: &[f64], t: f64) -> Option<f64> {
    if pts.is_empty() {
        return None;
    }
    let i = pts.partition_point(|p| *p < t);
    let cands = [i.checked_sub(1).map(|j| pts[j]), pts.get(i).copied()];
    cands.into_iter().flatten().min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_to_nearest_beat_and_bar() {
        let v = Value::map().with("bpm", 120.0).with("beats", vec![0.5f64, 1.0, 1.5, 2.0, 2.5]).with("downbeats", vec![0.5f64, 2.5]);
        let g = Grid::from_value(&v).unwrap();
        assert_eq!(g.snap(1.2, Snap::Beat), 1.0);
        assert_eq!(g.snap(1.3, Snap::Beat), 1.5);
        assert_eq!(g.snap(1.6, Snap::Bar), 2.5);
        assert_eq!(g.snap(9.0, Snap::Beat), 2.5, "beyond the grid: last beat");
        assert_eq!(g.snap(1.2, Snap::Off), 1.2);
        assert!(Grid::from_value(&Value::Null).is_none());
    }
}
