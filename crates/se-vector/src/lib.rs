//! GPU renderer for script-patch 2D draw lists ([`se_hub::draw::DrawList`]) built on
//! vello's compute renderer, with parley text and images from the project assets.
//!
//! # Coordinates
//! Draw lists are resolution independent. For a `W×H` target:
//! - points (`Rect`/`Image` `x,y`, `Circle` centre, `Line` ends, path points, text
//!   position, `Push` translate) map to `(x·W, y·H)`;
//! - `Rect`/`Image` box sizes map to `(w·W, h·H)`, so `(0, 0, 1, 1)` covers the layer;
//! - corner/circle radii, stroke and line widths and text size are fractions of the layer
//!   height (`·H`).
//!
//! `Push` composes `translate · rotate · scale` onto the current transform in layer
//! pixel space and multiplies the current alpha.
//!
//! # Output
//! [`VectorRenderer::render`] writes straight (non-premultiplied) alpha, sRGB-encoded
//! values: colors are taken as sRGB and blended in that space, as vello does.
//!
//! # Real time
//! `render` never blocks or touches the filesystem: images decode on a dedicated loader
//! thread and appear on a later render. The vello `Scene`, parley contexts, the path and
//! transform scratch buffers are reused, and shaped text is cached per string (LRU), so
//! steady-state per-call allocation is whatever vello's encoder/renderer need internally;
//! a string seen for the first time is shaped by parley on the calling thread. The
//! configured font family is resolved (fontconfig) in `new`/`set_options`; a fallback font
//! for a script not covered by it is first loaded when such text is first drawn.

mod images;
mod text;

use std::path::PathBuf;

use anyhow::{Context, bail};
use se_hub::draw::{DrawList, DrawOp, Paint, PathCmd};
use vello::kurbo::{Affine, BezPath, Cap, Circle, Join, Line, Point, Rect, RoundedRect, Shape, Stroke};
use vello::peniko::{Color, Fill, ImageBrush, ImageSampler};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};

use images::{ImageCache, Limits};
use text::TextEngine;

/// Renderer configuration; hot-reloadable through [`VectorRenderer::set_options`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorOptions {
    /// `DrawOp::Image` paths are relative to this directory (the project's `assets/`).
    pub assets_root: PathBuf,
    /// Font family for text; `None` (or a family that is not installed) falls back to the
    /// system sans-serif.
    pub font_family: Option<String>,
}

/// Texture usages a render target needs.
pub fn required_texture_usage() -> wgpu::TextureUsages {
    wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING
}

/// Push depth preallocated for the transform stack (the Lua API caps nesting at 64).
const STACK_CAPACITY: usize = 64;

/// Renders [`DrawList`]s into `Rgba8Unorm` storage textures. Create once per device;
/// `render` is meant for the render thread.
pub struct VectorRenderer {
    renderer: Renderer,
    scene: Scene,
    text: TextEngine,
    images: ImageCache,
    /// Saved `(transform, alpha)` for each open `Push`.
    stack: Vec<(Affine, f32)>,
    path: BezPath,
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<VectorRenderer>();
};

impl VectorRenderer {
    /// Build the vello pipelines (area AA only) and the text/image machinery.
    pub fn new(device: &wgpu::Device, opts: VectorOptions) -> anyhow::Result<VectorRenderer> {
        let renderer = Renderer::new(
            device,
            RendererOptions { use_cpu: false, antialiasing_support: AaSupport::area_only(), num_init_threads: None, pipeline_cache: None },
        )
        .context("creating the vello renderer")?;
        Ok(VectorRenderer {
            renderer,
            scene: Scene::new(),
            text: TextEngine::new(opts.font_family.as_deref()),
            images: ImageCache::new(&opts.assets_root, Limits::default())?,
            stack: Vec::with_capacity(STACK_CAPACITY),
            path: BezPath::new(),
        })
    }

    /// Apply new options (hot reload). Clears the image cache; images are reloaded from the
    /// new assets root on their next use.
    pub fn set_options(&mut self, opts: VectorOptions) {
        self.images.reset(&opts.assets_root);
        self.text.set_family(opts.font_family.as_deref());
    }

    /// Render `list` into `target` (Rgba8Unorm, usage STORAGE_BINDING|TEXTURE_BINDING, size[0]×size[1]).
    /// Output: straight (non-premultiplied) alpha, sRGB-encoded values, fully transparent where nothing is drawn (DrawOp::Clear sets the background).
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, list: &DrawList, target: &wgpu::TextureView, size: [u32; 2]) -> anyhow::Result<()> {
        let [width, height] = size;
        let texture = target.texture();
        if width == 0 || height == 0 {
            bail!("render size must be non-zero (got {width}×{height})");
        }
        if texture.format() != wgpu::TextureFormat::Rgba8Unorm {
            bail!("render target must be Rgba8Unorm (got {:?})", texture.format());
        }
        if !texture.usage().contains(wgpu::TextureUsages::STORAGE_BINDING) {
            bail!("render target needs STORAGE_BINDING usage (has {:?})", texture.usage());
        }
        if width > texture.width() || height > texture.height() {
            bail!("render size {width}×{height} exceeds the {}×{} target", texture.width(), texture.height());
        }
        self.images.poll();
        let base_color = self.encode(list, f64::from(width), f64::from(height));
        let params = RenderParams { base_color, width, height, antialiasing_method: AaConfig::Area };
        self.renderer.render_to_texture(device, queue, &self.scene, target, &params).context("vello render")
    }

    /// Images requested by draw lists that have not finished decoding yet.
    pub fn images_pending(&self) -> usize {
        self.images.pending()
    }

    /// Encode `list` into the scene; returns the background (last `Clear`) color.
    fn encode(&mut self, list: &DrawList, w: f64, h: f64) -> Color {
        self.scene.reset();
        self.stack.clear();
        // Drawing before the last Clear is invisible; transforms still apply across it.
        let (first_visible, base) = list
            .ops
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, op)| match op {
                DrawOp::Clear(c) => Some((i + 1, color(*c, 1.0))),
                _ => None,
            })
            .unwrap_or((0, Color::TRANSPARENT));
        let mut transform = Affine::IDENTITY;
        let mut alpha = 1.0f32;
        for (i, op) in list.ops.iter().enumerate() {
            match op {
                DrawOp::Push { translate, rotate, scale, alpha: a } => {
                    self.stack.push((transform, alpha));
                    if let Some(t) = push_transform(*translate, *rotate, *scale, w, h) {
                        transform *= t;
                    }
                    alpha *= unit(*a);
                }
                DrawOp::Pop => {
                    // An unbalanced Pop is ignored.
                    if let Some((t, a)) = self.stack.pop() {
                        transform = t;
                        alpha = a;
                    }
                }
                DrawOp::Clear(_) => {}
                _ if i < first_visible => {}
                op => self.draw(op, transform, alpha, w, h),
            }
        }
        base
    }

    fn draw(&mut self, op: &DrawOp, transform: Affine, alpha: f32, w: f64, h: f64) {
        let point = |p: [f32; 2]| Point::new(f64::from(p[0]) * w, f64::from(p[1]) * h);
        match op {
            DrawOp::Rect { xywh, radius, color: c, paint } => {
                let brush = color(*c, alpha);
                if brush.components[3] <= 0.0 || !finite(xywh) || !radius.is_finite() {
                    return;
                }
                let rect = Rect::new(f64::from(xywh[0]) * w, f64::from(xywh[1]) * h, f64::from(xywh[0] + xywh[2]) * w, f64::from(xywh[1] + xywh[3]) * h).abs();
                let radius = f64::from(radius.max(0.0)) * h;
                if radius > 0.0 {
                    paint_shape(&mut self.scene, &RoundedRect::from_rect(rect, radius), paint, transform, brush, h);
                } else {
                    paint_shape(&mut self.scene, &rect, paint, transform, brush, h);
                }
            }
            DrawOp::Circle { center, radius, color: c, paint } => {
                let brush = color(*c, alpha);
                if brush.components[3] <= 0.0 || !finite(center) || !(radius.is_finite() && *radius > 0.0) {
                    return;
                }
                paint_shape(&mut self.scene, &Circle::new(point(*center), f64::from(*radius) * h), paint, transform, brush, h);
            }
            DrawOp::Line { a, b, width, color: c } => {
                let brush = color(*c, alpha);
                if brush.components[3] <= 0.0 || !finite(a) || !finite(b) {
                    return;
                }
                paint_shape(&mut self.scene, &Line::new(point(*a), point(*b)), &Paint::Stroke(*width), transform, brush, h);
            }
            DrawOp::Path { cmds, color: c, paint } => {
                let brush = color(*c, alpha);
                if brush.components[3] <= 0.0 || !build_path(&mut self.path, cmds, w, h) {
                    return;
                }
                paint_shape(&mut self.scene, &self.path, paint, transform, brush, h);
            }
            DrawOp::Text { pos, size, color: c, text, align } => {
                let brush = color(*c, alpha);
                if brush.components[3] <= 0.0 || text.is_empty() || !finite(pos) || !(size.is_finite() && *size > 0.0) {
                    return;
                }
                self.text.draw(&mut self.scene, transform, text, point(*pos), f64::from(*size) * h, *align, brush);
            }
            DrawOp::Image { path, xywh, opacity } => {
                if !finite(xywh) {
                    return;
                }
                // Look up (and so request) even when invisible, so fade-ins find it loaded.
                let Some(image) = self.images.get(path) else {
                    return;
                };
                let a = unit(*opacity) * alpha;
                let (bw, bh) = (f64::from(xywh[2]) * w, f64::from(xywh[3]) * h);
                if a <= 0.0 || bw == 0.0 || bh == 0.0 {
                    return;
                }
                let placement = Affine::translate((f64::from(xywh[0]) * w, f64::from(xywh[1]) * h))
                    * Affine::scale_non_uniform(bw / f64::from(image.width), bh / f64::from(image.height));
                let brush = ImageBrush { image, sampler: ImageSampler::default().with_alpha(a) };
                self.scene.draw_image(brush, transform * placement);
            }
            DrawOp::Clear(_) | DrawOp::Push { .. } | DrawOp::Pop => {}
        }
    }
}

/// Fill (non-zero) or stroke (butt caps, miter joins) `shape`.
fn paint_shape(scene: &mut Scene, shape: &impl Shape, paint: &Paint, transform: Affine, brush: Color, h: f64) {
    match *paint {
        Paint::Fill => scene.fill(Fill::NonZero, transform, brush, None, shape),
        Paint::Stroke(width) => {
            if width.is_finite() && width > 0.0 {
                let stroke = Stroke::new(f64::from(width) * h).with_caps(Cap::Butt).with_join(Join::Miter);
                scene.stroke(&stroke, transform, brush, None, shape);
            }
        }
    }
}

/// Rebuild `path` from draw commands in layer pixels. Returns false for non-finite or
/// empty paths. A segment without a current point starts a subpath (at the previous
/// subpath's start after a `Close`, as in SVG, else at the segment's end point).
fn build_path(path: &mut BezPath, cmds: &[PathCmd], w: f64, h: f64) -> bool {
    let point = |p: [f32; 2]| Point::new(f64::from(p[0]) * w, f64::from(p[1]) * h);
    let all_finite = cmds.iter().all(|c| match c {
        PathCmd::MoveTo(p) | PathCmd::LineTo(p) => finite(p),
        PathCmd::QuadTo(a, b) => finite(a) && finite(b),
        PathCmd::CubicTo(a, b, c) => finite(a) && finite(b) && finite(c),
        PathCmd::Close => true,
    });
    if !all_finite {
        return false;
    }
    path.truncate(0);
    let mut start: Option<Point> = None;
    let mut open = false;
    for cmd in cmds {
        let end = match cmd {
            PathCmd::MoveTo(p) => {
                let p = point(*p);
                path.move_to(p);
                start = Some(p);
                open = true;
                continue;
            }
            PathCmd::Close => {
                if open {
                    path.close_path();
                    open = false;
                }
                continue;
            }
            PathCmd::LineTo(p) | PathCmd::QuadTo(_, p) | PathCmd::CubicTo(_, _, p) => point(*p),
        };
        if !open {
            let s = start.unwrap_or(end);
            path.move_to(s);
            start = Some(s);
            open = true;
        }
        match cmd {
            PathCmd::LineTo(_) => path.line_to(end),
            PathCmd::QuadTo(c, _) => path.quad_to(point(*c), end),
            PathCmd::CubicTo(c1, c2, _) => path.curve_to(point(*c1), point(*c2), end),
            PathCmd::MoveTo(_) | PathCmd::Close => {}
        }
    }
    !path.elements().is_empty()
}

fn push_transform(translate: [f32; 2], rotate: f32, scale: [f32; 2], w: f64, h: f64) -> Option<Affine> {
    if !(finite(&translate) && rotate.is_finite() && finite(&scale)) {
        return None;
    }
    Some(
        Affine::translate((f64::from(translate[0]) * w, f64::from(translate[1]) * h))
            * Affine::rotate(f64::from(rotate))
            * Affine::scale_non_uniform(f64::from(scale[0]), f64::from(scale[1])),
    )
}

fn finite<const N: usize>(v: &[f32; N]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// Clamp to 0–1, NaN → 0.
fn unit(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// An sRGB color with its alpha multiplied by `alpha`.
fn color(c: [f32; 4], alpha: f32) -> Color {
    Color::new([unit(c[0]), unit(c[1]), unit(c[2]), unit(c[3]) * alpha])
}

#[cfg(test)]
mod tests {
    use super::*;
    use vello::kurbo::PathEl;

    #[test]
    fn path_segments_without_current_point() {
        let mut path = BezPath::new();
        let cmds = [PathCmd::LineTo([0.5, 0.5]), PathCmd::LineTo([1.0, 0.5]), PathCmd::Close, PathCmd::LineTo([1.0, 1.0])];
        assert!(build_path(&mut path, &cmds, 100.0, 10.0));
        let p = |x, y| Point::new(x, y);
        assert_eq!(
            path.elements(),
            &[
                PathEl::MoveTo(p(50.0, 5.0)),
                PathEl::LineTo(p(50.0, 5.0)),
                PathEl::LineTo(p(100.0, 5.0)),
                PathEl::ClosePath,
                PathEl::MoveTo(p(50.0, 5.0)),
                PathEl::LineTo(p(100.0, 10.0)),
            ]
        );
    }

    #[test]
    fn non_finite_or_empty_paths_are_skipped() {
        let mut path = BezPath::new();
        assert!(!build_path(&mut path, &[], 1.0, 1.0));
        assert!(!build_path(&mut path, &[PathCmd::Close], 1.0, 1.0));
        assert!(!build_path(&mut path, &[PathCmd::MoveTo([0.0, 0.0]), PathCmd::LineTo([f32::NAN, 0.0])], 1.0, 1.0));
    }

    #[test]
    fn colors_are_sanitized() {
        let c = color([f32::NAN, 2.0, -1.0, 0.5], 0.5);
        assert_eq!(c.components, [0.0, 1.0, 0.0, 0.25]);
    }
}
