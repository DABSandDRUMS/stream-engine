//! Text ops: shaping and line layout with parley (system fonts through fontique /
//! fontconfig), encoded as vello glyph runs.
//!
//! Shaping is by far the most expensive part of a draw list, and patches redraw the same
//! strings every frame, so shaped text is cached. Text is shaped once at [`SHAPE_PX`] and
//! scaled to the requested size by the glyph transform (outlines are unhinted, so this
//! matches shaping at the target size and animated sizes stay cache hits). A cache hit
//! does not allocate; a miss reuses the evicted entry's buffers.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::ops::Range;

use parley::{
    Alignment, AlignmentOptions, FontContext, FontData, FontFamily, FontFamilyName, GenericFamily, Layout, LayoutContext, PositionedLayoutItem, StyleProperty,
};
use se_hub::draw::TextAlign;
use vello::kurbo::{Affine, Point};
use vello::peniko::{Color, Fill};
use vello::{Glyph, NormalizedCoord, Scene};

/// Font size text is shaped at before scaling.
const SHAPE_PX: f32 = 64.0;
/// Font sizes are clamped to this many pixels (far beyond any layer; keeps glyph
/// coordinates finite for absurd inputs).
const MAX_FONT_PX: f64 = 16384.0;
/// Shaped strings kept (least recently used beyond this).
const CACHE_ENTRIES: usize = 256;

struct ShapedRun {
    font: FontData,
    font_size: f32,
    coords: Range<usize>,
    glyphs: Range<usize>,
}

struct ShapedLine {
    /// Advance without trailing whitespace, at [`SHAPE_PX`].
    width: f32,
    runs: Range<usize>,
}

#[derive(Default)]
struct Shaped {
    text: String,
    first_baseline: f32,
    lines: Vec<ShapedLine>,
    runs: Vec<ShapedRun>,
    coords: Vec<NormalizedCoord>,
    /// Positions relative to the layout's top-left, at [`SHAPE_PX`].
    glyphs: Vec<Glyph>,
    last_used: u64,
}

impl Shaped {
    fn fill(&mut self, layout: &Layout<()>, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.lines.clear();
        self.runs.clear();
        self.coords.clear();
        self.glyphs.clear();
        self.first_baseline = layout.lines().next().map_or(0.0, |l| l.metrics().baseline);
        for line in layout.lines() {
            let first_run = self.runs.len();
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let run = glyph_run.run();
                let (c0, g0) = (self.coords.len(), self.glyphs.len());
                self.coords.extend_from_slice(run.normalized_coords());
                self.glyphs.extend(glyph_run.positioned_glyphs().map(|g| Glyph { id: g.id, x: g.x, y: g.y }));
                self.runs.push(ShapedRun {
                    font: run.font().clone(),
                    font_size: run.font_size(),
                    coords: c0..self.coords.len(),
                    glyphs: g0..self.glyphs.len(),
                });
            }
            let metrics = line.metrics();
            self.lines.push(ShapedLine { width: metrics.advance - metrics.trailing_whitespace, runs: first_run..self.runs.len() });
        }
    }
}

pub(crate) struct TextEngine {
    fonts: FontContext,
    layouts: LayoutContext<()>,
    /// Reused for every shaping (`build_into` keeps its allocations).
    layout: Layout<()>,
    /// The configured family followed by generic sans-serif.
    families: Vec<FontFamilyName<'static>>,
    /// Keyed by the hash of the text; the entry's `text` is compared on lookup.
    cache: HashMap<u64, Shaped>,
    hash: RandomState,
    tick: u64,
}

impl TextEngine {
    pub(crate) fn new(family: Option<&str>) -> Self {
        let mut engine = Self {
            fonts: FontContext::new(),
            layouts: LayoutContext::new(),
            layout: Layout::new(),
            families: Vec::with_capacity(2),
            cache: HashMap::with_capacity(CACHE_ENTRIES + 1),
            hash: RandomState::new(),
            tick: 0,
        };
        engine.set_family(family);
        engine
    }

    pub(crate) fn set_family(&mut self, family: Option<&str>) {
        self.families.clear();
        if let Some(name) = family.map(str::trim).filter(|n| !n.is_empty()) {
            self.families.push(FontFamilyName::Named(Cow::Owned(name.to_owned())));
        }
        self.families.push(FontFamilyName::Generic(GenericFamily::SansSerif));
        self.cache.clear();
        // Resolve the family through fontconfig and map its font file now, so the first
        // text op on the render thread does not pay for it.
        self.shape("Ag");
    }

    fn shape(&mut self, text: &str) {
        let mut builder = self.layouts.ranged_builder(&mut self.fonts, text, 1.0, false);
        builder.push_default(StyleProperty::FontSize(SHAPE_PX));
        builder.push_default(StyleProperty::FontFamily(FontFamily::List(Cow::Borrowed(&self.families))));
        builder.build_into(&mut self.layout, text);
        self.layout.break_all_lines(None);
        self.layout.align(Alignment::Left, AlignmentOptions::default());
    }

    /// Shape `text` into the cache under `key`, reusing the buffers of the entry it
    /// replaces (a hash collision or the least recently used one).
    fn shape_into_cache(&mut self, key: u64, text: &str) {
        let mut entry = match self.cache.remove(&key) {
            Some(entry) => entry,
            None if self.cache.len() >= CACHE_ENTRIES => {
                let lru = self.cache.iter().min_by_key(|(_, s)| s.last_used).map(|(k, _)| *k);
                lru.and_then(|k| self.cache.remove(&k)).unwrap_or_default()
            }
            None => Shaped::default(),
        };
        self.shape(text);
        entry.fill(&self.layout, text);
        self.cache.insert(key, entry);
    }

    /// Draw `text` at `size_px` with the first line's baseline starting at
    /// `baseline_start` (layer pixels, before `transform`). Every line is aligned about
    /// `baseline_start.x`.
    pub(crate) fn draw(&mut self, scene: &mut Scene, transform: Affine, text: &str, baseline_start: Point, size_px: f64, align: TextAlign, color: Color) {
        let key = self.hash.hash_one(text);
        if !self.cache.get(&key).is_some_and(|s| s.text == text) {
            self.shape_into_cache(key, text);
        }
        let Some(shaped) = self.cache.get_mut(&key) else {
            return;
        };
        self.tick += 1;
        shaped.last_used = self.tick;

        let scale = size_px.min(MAX_FONT_PX) / f64::from(SHAPE_PX);
        let top = baseline_start.y - f64::from(shaped.first_baseline) * scale;
        for line in &shaped.lines {
            let width = f64::from(line.width) * scale;
            let dx = match align {
                TextAlign::Left => 0.0,
                TextAlign::Center => -width / 2.0,
                TextAlign::Right => -width,
            };
            let line_transform = transform * Affine::translate((baseline_start.x + dx, top)) * Affine::scale(scale);
            for run in &shaped.runs[line.runs.clone()] {
                scene
                    .draw_glyphs(&run.font)
                    .font_size(run.font_size)
                    .transform(line_transform)
                    .normalized_coords(&shaped.coords[run.coords.clone()])
                    .brush(color)
                    .draw(Fill::NonZero, shaped.glyphs[run.glyphs.clone()].iter().copied());
            }
        }
    }

    #[cfg(test)]
    fn cached(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(engine: &mut TextEngine, scene: &mut Scene, text: &str, size_px: f64) {
        engine.draw(scene, Affine::IDENTITY, text, Point::new(10.0, 50.0), size_px, TextAlign::Left, Color::new([1.0; 4]));
    }

    #[test]
    fn shaped_text_is_cached_across_sizes() {
        let mut engine = TextEngine::new(None);
        let mut scene = Scene::new();
        draw(&mut engine, &mut scene, "Hello", 20.0);
        draw(&mut engine, &mut scene, "Hello", 40.0);
        draw(&mut engine, &mut scene, "Hello", 40.0);
        assert_eq!(engine.cached(), 1, "one entry per string regardless of size");
        draw(&mut engine, &mut scene, "World", 40.0);
        assert_eq!(engine.cached(), 2);
        engine.set_family(Some("Liberation Sans"));
        assert_eq!(engine.cached(), 0, "a family change invalidates shaped text");
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let mut engine = TextEngine::new(None);
        let mut scene = Scene::new();
        for i in 0..CACHE_ENTRIES {
            draw(&mut engine, &mut scene, &format!("t{i}"), 10.0);
        }
        // Touch t0 so t1 is the oldest, then overflow by one.
        draw(&mut engine, &mut scene, "t0", 10.0);
        draw(&mut engine, &mut scene, "new", 10.0);
        assert_eq!(engine.cached(), CACHE_ENTRIES);
        let has = |e: &TextEngine, t: &str| e.cache.values().any(|s| s.text == t);
        assert!(has(&engine, "t0") && has(&engine, "new") && !has(&engine, "t1"));
    }

    #[test]
    fn lines_and_glyphs_are_recorded() {
        let mut engine = TextEngine::new(None);
        let mut scene = Scene::new();
        draw(&mut engine, &mut scene, "ab\ncd", 10.0);
        let shaped = engine.cache.values().find(|s| s.text == "ab\ncd").unwrap();
        assert_eq!(shaped.lines.len(), 2);
        assert!(shaped.lines.iter().all(|l| l.width > 0.0));
        assert!(shaped.glyphs.len() >= 4);
        assert!(shaped.first_baseline > 0.0);
    }
}
