//! Fusion of adjacent pointwise effects (§4.1 "adjacent simple effects are fused where
//! possible"). At load and on hot reload, every static effect chain of the plan (source,
//! node, scene, canvas + global canvas, output + global output) is projected onto its
//! [`Exec::Pointwise`] effects; each projection with two or more effects gets one fused pipeline
//! (compiled on the loader thread, never per frame). Per frame, a run of consecutive active
//! pointwise effects is an ordered subset of its chain's projection, so it runs through that
//! pipeline with the inactive stages at strength 0. Everything else (blur, LUT, effects that
//! sample neighbours, shader patches) keeps its own pass.

use crate::compose::FxEval;
use crate::effects::{Exec, LIBRARY, MAX_FUSED};
use crate::plan::{Attach, CANVASES, EffectKind, Plan};

/// A compiled fused chain (library indices in stage order).
pub struct FusedPipe {
    pub chain: Box<[u8]>,
    pub pipeline: wgpu::RenderPipeline,
}

/// Library index of an attachment's effect when it is fusable.
fn pointwise(plan: &Plan, effect: usize) -> Option<u8> {
    match plan.effects[effect].kind {
        EffectKind::Builtin(i) if LIBRARY[i].exec == Exec::Pointwise => Some(i as u8),
        _ => None,
    }
}

/// Distinct fusable chains of `plan` (each 2..=[`MAX_FUSED`] library indices), shortest first.
pub fn chains(plan: &Plan) -> Vec<Vec<u8>> {
    let globals = [plan.global_attaches(crate::effects::Point::Canvas), plan.global_attaches(crate::effects::Point::Output)];
    let mut lists: Vec<[&[Attach]; 2]> = Vec::new();
    for s in &plan.sources {
        lists.push([&s.fx, &[]]);
    }
    for sc in &plan.scenes {
        for l in &sc.layouts {
            lists.push([&l.fx, &[]]);
            for n in &l.nodes {
                lists.push([&n.fx, &[]]);
            }
        }
    }
    for c in plan.canvases.iter().take(CANVASES) {
        lists.push([&c.fx, &globals[0]]);
        lists.push([&c.output_fx, &globals[1]]);
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    for [a, b] in lists {
        let projection: Vec<u8> = a.iter().chain(b.iter()).filter_map(|at| pointwise(plan, at.effect)).collect();
        for chunk in projection.chunks(MAX_FUSED) {
            if chunk.len() >= 2 && !out.iter().any(|c| c == chunk) {
                out.push(chunk.to_vec());
            }
        }
    }
    out.sort_by_key(Vec::len);
    out
}

/// Stage slot per effect of a fused run.
pub type Slots = [u8; MAX_FUSED];

/// The longest prefix (≥ 2) of `run` (library indices of consecutive pointwise effects) that one
/// chain covers as an ordered subsequence: `(chain, prefix length, stage slot per effect)`.
/// Ties go to the shorter chain (fewer skipped stages).
pub fn cover<'a>(chains: impl Iterator<Item = &'a [u8]>, run: &[u8]) -> Option<(usize, usize, Slots)> {
    let mut best: Option<(usize, usize, usize, Slots)> = None;
    for (ci, chain) in chains.enumerate() {
        let mut slots = [0u8; MAX_FUSED];
        let (mut n, mut k) = (0, 0);
        while n < run.len().min(MAX_FUSED) {
            match chain[k..].iter().position(|&e| e == run[n]) {
                Some(p) => {
                    k += p;
                    slots[n] = k as u8;
                    k += 1;
                    n += 1;
                }
                None => break,
            }
        }
        if n >= 2 && best.is_none_or(|(_, bn, bl, _)| n > bn || (n == bn && chain.len() < bl)) {
            best = Some((ci, n, chain.len(), slots));
        }
    }
    best.map(|(ci, n, _, slots)| (ci, n, slots))
}

/// One pass of an evaluated effect chain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// `evals[i]` on its own.
    Single(usize),
    /// `evals[first..first + len]` through fused pipeline `pipe`, effect `j` at stage `slots[j]`.
    Fused { first: usize, len: usize, pipe: usize, slots: Slots },
}

/// Split this frame's evaluated chain into passes (no allocation).
pub struct Steps<'a> {
    plan: &'a Plan,
    evals: &'a [FxEval],
    fused: &'a [FusedPipe],
    i: usize,
}

impl<'a> Steps<'a> {
    pub fn new(plan: &'a Plan, evals: &'a [FxEval], fused: &'a [FusedPipe]) -> Steps<'a> {
        Steps { plan, evals, fused, i: 0 }
    }
}

impl Iterator for Steps<'_> {
    type Item = Step;

    fn next(&mut self) -> Option<Step> {
        let i = self.i;
        if i >= self.evals.len() {
            return None;
        }
        let lib = |e: &FxEval| pointwise(self.plan, e.effect);
        if !self.fused.is_empty() && lib(&self.evals[i]).is_some() {
            let mut run = [0u8; MAX_FUSED];
            let mut n = 0;
            while n < MAX_FUSED
                && let Some(l) = self.evals.get(i + n).and_then(lib)
            {
                run[n] = l;
                n += 1;
            }
            if n >= 2
                && let Some((pipe, len, slots)) = cover(self.fused.iter().map(|f| &*f.chain), &run[..n])
            {
                self.i += len;
                return Some(Step::Fused { first: i, len, pipe, slots });
            }
        }
        self.i += 1;
        Some(Step::Single(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::find;
    use crate::plan::tests::config;
    use std::path::PathBuf;

    fn lib(n: &str) -> u8 {
        find(n).unwrap() as u8
    }

    #[test]
    fn cover_takes_the_longest_prefix_then_the_shortest_chain() {
        let (g, v, c, f) = (lib("grade"), lib("vignette"), lib("chroma_key"), lib("fade_to_black"));
        let long: &[u8] = &[c, g, f, v];
        let short: &[u8] = &[g, v];
        // run [g, v] is covered by both; the shorter chain wins, slots follow its positions
        assert_eq!(cover([long, short].into_iter(), &[g, v]), Some((1, 2, [0, 1, 0, 0, 0, 0, 0, 0])));
        // a subset with a gap uses the long chain's slots (the skipped stage stays at strength 0)
        assert_eq!(cover([short, long].into_iter(), &[c, v]), Some((1, 2, [0, 3, 0, 0, 0, 0, 0, 0])));
        // order matters: [v, g] is no subsequence of either → nothing to fuse
        assert_eq!(cover([long, short].into_iter(), &[v, g]), None);
        // only a prefix fits: [g, v, c] fuses [g, v], c runs on its own afterwards
        assert_eq!(cover([long, short].into_iter(), &[g, v, c]).map(|(ci, n, _)| (ci, n)), Some((1, 2)));
        // the same effect twice needs two stages of it
        assert_eq!(cover([short].into_iter(), &[g, g]), None);
    }

    #[test]
    fn chains_project_every_static_chain_onto_its_fusable_effects() {
        let c = config(&[
            ("project", "project", "schema = 1"),
            (
                "scenes",
                "s",
                "fx = [{ name = \"grade\", warmth = 0.2 }, { name = \"blur\" }, { name = \"vignette\" }, { name = \"chroma_key\" }]\n[canvas.wide]\nnodes = [{ id = \"a\", src = \"color:#ff0000\", fx = [{ name = \"vignette\" }] }]",
            ),
        ]);
        let p = Plan::build(&c, &[], PathBuf::from("/tmp"));
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        let ch = chains(&p);
        let (g, v, k, f) = (lib("grade"), lib("vignette"), lib("chroma_key"), lib("fade_to_black"));
        // scene chain: blur is not fusable, so the projection skips it (fused when blur is off)
        assert!(ch.contains(&vec![g, v, k]), "{ch:?}");
        // global canvas chain: grade … lut … vignette → [grade, vignette]
        assert!(ch.contains(&vec![g, v]), "{ch:?}");
        // single-effect chains (node vignette, output fade_to_black alone) get no fused pipeline
        assert!(ch.iter().all(|c| c.len() >= 2 && c.len() <= MAX_FUSED));
        assert!(!ch.iter().any(|c| c == &vec![f]));
        assert!(ch.windows(2).all(|w| w[0].len() <= w[1].len()), "shortest first");
    }
}
