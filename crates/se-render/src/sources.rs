//! Video sources → GPU textures (§4.2). CPU frames from `hub.video` slots are copied into
//! persistently mapped system-RAM staging buffers (reused, never created per frame), uploaded
//! with `copy_buffer_to_texture`, and converted in one fragment pass (YUYV/NV12 → RGB, straight →
//! premultiplied alpha, per-source color correction and 3D LUT) into a premultiplied RGBA
//! texture that every placement samples. Sources nobody shows are not uploaded.

use crate::addr::Resolved;
use crate::gpu::Gpu;
use crate::plan::ColorAddrs;
use crate::resources::{Arena, BindCache, Layouts, Tex};
use crate::upload::UploadRing;
use se_hub::Snapshot;
use se_hub::media::{PixelFormat, VideoFrame};

pub const KIND_CONVERT: u8 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ConvertUniform {
    pub mode: u32,
    pub matrix: u32,
    pub full_range: u32,
    pub lut_on: u32,
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub gamma: f32,
    pub temperature: f32,
    pub tint: f32,
    pub lut_amount: f32,
    pub width: f32,
}

fn align256(n: u32) -> u32 {
    n.div_ceil(256) * 256
}

/// Upload geometry for one frame format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// Texel size of plane 0 / plane 1.
    pub plane: [[u32; 2]; 2],
    /// Padded bytes per row per plane in the staging buffer.
    pub row: [u32; 2],
    /// Bytes per row actually copied per plane.
    pub copy: [u32; 2],
    pub plane1_offset: u64,
    pub staging_size: u64,
}

impl Layout {
    pub fn new(format: PixelFormat, width: u32, height: u32) -> Layout {
        let (w, h) = (width.max(2), height.max(2));
        match format {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => {
                let row = align256(w * 4);
                Layout {
                    format,
                    width: w,
                    height: h,
                    plane: [[w, h], [0, 0]],
                    row: [row, 0],
                    copy: [w * 4, 0],
                    plane1_offset: 0,
                    staging_size: row as u64 * h as u64,
                }
            }
            PixelFormat::Yuyv => {
                let tw = w.div_ceil(2);
                let row = align256(tw * 4);
                Layout {
                    format,
                    width: w,
                    height: h,
                    plane: [[tw, h], [0, 0]],
                    row: [row, 0],
                    copy: [tw * 4, 0],
                    plane1_offset: 0,
                    staging_size: row as u64 * h as u64,
                }
            }
            PixelFormat::Nv12 => {
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let r0 = align256(w);
                let r1 = align256(cw * 2);
                let off = r0 as u64 * h as u64;
                Layout {
                    format,
                    width: w,
                    height: h,
                    plane: [[w, h], [cw, ch]],
                    row: [r0, r1],
                    copy: [w, cw * 2],
                    plane1_offset: off,
                    staging_size: off + r1 as u64 * ch as u64,
                }
            }
        }
    }

    pub fn texture_formats(&self) -> [wgpu::TextureFormat; 2] {
        match self.format {
            PixelFormat::Rgba8 | PixelFormat::Yuyv => [wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureFormat::R8Unorm],
            PixelFormat::Bgra8 => [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::R8Unorm],
            PixelFormat::Nv12 => [wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm],
        }
    }

    /// Copy a CPU frame into the padded staging layout. Returns false if the frame is short.
    pub fn fill<S: Sink + ?Sized>(&self, f: &VideoFrame, dst: &mut S) -> bool {
        let stride = f.stride as usize;
        let rows0 = self.plane[0][1] as usize;
        let need0 = stride * (rows0 - 1) + self.copy[0] as usize;
        if f.data.len() < need0 || stride < self.copy[0] as usize {
            return false;
        }
        copy_plane(&f.data, stride, dst, 0, self.row[0] as usize, self.copy[0] as usize, rows0);
        if self.format == PixelFormat::Nv12 {
            let src_off = stride * rows0;
            let rows1 = self.plane[1][1] as usize;
            if f.data.len() < src_off + stride * (rows1 - 1) + self.copy[1] as usize {
                return false;
            }
            copy_plane(&f.data[src_off..], stride, dst, self.plane1_offset as usize, self.row[1] as usize, self.copy[1] as usize, rows1);
        }
        true
    }
}

/// Destination of staging copies: plain memory (tests) or a persistently mapped system buffer.
pub trait Sink {
    fn put(&mut self, offset: usize, src: &[u8]);
}

impl Sink for [u8] {
    fn put(&mut self, offset: usize, src: &[u8]) {
        self[offset..offset + src.len()].copy_from_slice(src);
    }
}

fn copy_plane<S: Sink + ?Sized>(src: &[u8], stride: usize, dst: &mut S, base: usize, row: usize, bytes: usize, rows: usize) {
    if stride == row && src.len() >= row * rows {
        dst.put(base, &src[..row * rows]);
        return;
    }
    for y in 0..rows {
        dst.put(base + y * row, &src[y * stride..y * stride + bytes]);
    }
}

pub fn color_settings(snap: &Snapshot, res: &Resolved, a: &ColorAddrs, format: PixelFormat, width: u32, lut_ready: bool) -> ConvertUniform {
    let matrix = match res.str(snap, a.matrix) {
        Some("bt601") => 0,
        Some("bt709") => 1,
        // HD and larger default to BT.709, SD to BT.601
        _ => u32::from(width >= 1280),
    };
    let full_range = match res.str(snap, a.range) {
        Some("full") => 1,
        Some("limited") => 0,
        _ => 0,
    };
    let lut_amount = res.f32(snap, a.lut_amount, 1.0).clamp(0.0, 1.0);
    ConvertUniform {
        mode: match format {
            PixelFormat::Rgba8 => 0,
            PixelFormat::Bgra8 => 1,
            PixelFormat::Yuyv => 2,
            PixelFormat::Nv12 => 3,
        },
        matrix,
        full_range,
        lut_on: u32::from(lut_ready && lut_amount > 0.0),
        brightness: res.f32(snap, a.brightness, 0.0).clamp(-1.0, 1.0),
        contrast: res.f32(snap, a.contrast, 1.0).clamp(0.0, 4.0),
        saturation: res.f32(snap, a.saturation, 1.0).clamp(0.0, 4.0),
        gamma: res.f32(snap, a.gamma, 1.0).clamp(0.05, 10.0),
        temperature: res.f32(snap, a.temperature, 0.0).clamp(-1.0, 1.0),
        tint: res.f32(snap, a.tint, 0.0).clamp(-1.0, 1.0),
        lut_amount,
        width: width as f32,
    }
}

/// GPU side of one `hub.video` slot.
#[derive(Default)]
pub struct VideoGpu {
    pub layout: Option<Layout>,
    planes: [Option<Tex>; 2],
    staging: Option<UploadRing>,
    /// Converted, premultiplied RGBA at the source size.
    pub out: Option<Tex>,
    pub has_frame: bool,
    pub last_seq: u64,
    /// Frame uploaded but not converted yet (or settings changed).
    dirty: bool,
    last_settings: Option<ConvertUniform>,
    last_lut: u32,
    pub bytes: u64,
    pub uploads: u64,
    pub skipped: u64,
}

impl VideoGpu {
    fn ensure(&mut self, device: &wgpu::Device, binds: &mut BindCache, name: &str, layout: Layout) -> anyhow::Result<()> {
        if self.layout == Some(layout) {
            return Ok(());
        }
        let staging = UploadRing::new(device, &format!("{name} staging"), layout.staging_size)?;
        for t in self.planes.iter().chain(std::iter::once(&self.out)).flatten() {
            binds.forget(t.id);
        }
        let fmts = layout.texture_formats();
        let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        self.planes[0] = Some(Tex::new(device, &format!("{name} plane0"), layout.plane[0], fmts[0], usage));
        self.planes[1] = (layout.plane[1][0] > 0).then(|| Tex::new(device, &format!("{name} plane1"), layout.plane[1], fmts[1], usage));
        self.staging = Some(staging);
        self.out = Some(Tex::target(device, name, [layout.width, layout.height]));
        self.bytes = layout.staging_size * 4 + self.out.as_ref().map_or(0, Tex::bytes);
        self.layout = Some(layout);
        self.has_frame = false;
        self.last_settings = None;
        Ok(())
    }

    /// Upload the newest frame (if any) into the plane textures. Returns true if a new frame
    /// was uploaded.
    pub fn upload(&mut self, gpu: &Gpu, enc: &mut wgpu::CommandEncoder, binds: &mut BindCache, name: &str, frame: Option<&VideoFrame>) -> bool {
        let Some(f) = frame else { return false };
        if f.seq == self.last_seq || f.width == 0 || f.height == 0 {
            return false;
        }
        let layout = Layout::new(f.format, f.width, f.height);
        if self.layout != Some(layout) {
            let _p = se_alloc::Pause::new();
            if let Err(error) = self.ensure(&gpu.device, binds, name, layout) {
                *gpu.lost_reason.lock() = format!("source {name} upload allocation failed: {error:#}");
                gpu.lost.store(true, std::sync::atomic::Ordering::Release);
                tracing::error!(target: "render", "source {name} upload allocation failed: {error:#}; recovering GPU resources");
                return false;
            }
        }
        let ring = self.staging.as_mut().expect("ensured");
        // queue completion callbacks allocate inside wgpu (GPU API, excluded).
        let _p = se_alloc::Pause::new();
        let Some(i) = ring.acquire() else {
            // all staging buffers still in flight: keep the previous frame this time
            self.skipped += 1;
            return false;
        };
        let ok = layout.fill(f, ring.write(i));
        if !ok {
            // The slot is returned after submission even if this malformed frame wasn't copied.
            self.skipped += 1;
            return false;
        }
        let buf = ring.buffer(i);
        for p in 0..2 {
            let Some(tex) = &self.planes[p] else { continue };
            enc.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: if p == 0 { 0 } else { layout.plane1_offset },
                        bytes_per_row: Some(layout.row[p]),
                        rows_per_image: Some(layout.plane[p][1]),
                    },
                },
                tex.texture.as_image_copy(),
                wgpu::Extent3d { width: layout.plane[p][0], height: layout.plane[p][1], depth_or_array_layers: 1 },
            );
        }
        self.last_seq = f.seq;
        self.dirty = true;
        self.uploads += 1;
        true
    }

    /// Convert into `out` if a new frame arrived or the color settings changed.
    #[allow(clippy::too_many_arguments)]
    pub fn convert(
        &mut self,
        device: &wgpu::Device,
        enc: &mut wgpu::CommandEncoder,
        layouts: &Layouts,
        pipeline: &wgpu::RenderPipeline,
        arena: &mut Arena,
        binds: &mut BindCache,
        settings: ConvertUniform,
        lut: Option<(u32, &wgpu::TextureView)>,
    ) {
        let (Some(out), Some(p0)) = (&self.out, &self.planes[0]) else { return };
        let lut_id = lut.map_or(0, |l| l.0);
        if !self.dirty && self.last_settings == Some(settings) && self.last_lut == lut_id {
            return;
        }
        let off = arena.push_pod(&settings);
        let p1 = self.planes[1].as_ref();
        let bg = binds.get_or((KIND_CONVERT, p0.id, p1.map_or(0, |t| t.id), lut_id), || {
            let _p = se_alloc::Pause::new();
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("convert"),
                layout: &layouts.convert,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: arena.binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&p0.view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(p1.map_or(&layouts.dummy_2d, |t| &t.view)) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(lut.map_or(&layouts.dummy_3d, |l| l.1)) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&layouts.linear) },
                ],
            })
        });
        let _p = se_alloc::Pause::new();
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("convert"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &out.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bg, &[off]);
        pass.draw(0..3, 0..1);
        drop(pass);
        self.dirty = false;
        self.has_frame = true;
        self.last_settings = Some(settings);
        self.last_lut = lut_id;
    }

    pub fn after_submit(&mut self, queue: &wgpu::Queue) {
        if let Some(r) = &mut self.staging {
            r.after_submit(queue);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_256_aligned() {
        let y = Layout::new(PixelFormat::Yuyv, 1920, 1080);
        assert_eq!(y.plane[0], [960, 1080]);
        assert_eq!(y.row[0], 3840);
        let n = Layout::new(PixelFormat::Nv12, 1280, 721);
        assert_eq!(n.plane[1], [640, 361]);
        assert_eq!(n.row, [1280, 1280]);
        assert_eq!(n.plane1_offset, 1280 * 721);
        let r = Layout::new(PixelFormat::Rgba8, 1080, 1920);
        assert_eq!(r.row[0], 4352);
        assert_eq!(r.copy[0], 4320);
    }

    fn frame(format: PixelFormat, w: u32, h: u32, stride: u32) -> VideoFrame {
        let len = match format {
            PixelFormat::Nv12 => stride * h + stride * h.div_ceil(2),
            _ => stride * h,
        } as usize;
        VideoFrame { width: w, height: h, stride, format, data: (0..len).map(|i| (i % 251) as u8).collect(), seq: 1, ts: 0 }
    }

    #[test]
    fn fill_repacks_rows() {
        let f = frame(PixelFormat::Rgba8, 10, 3, 48);
        let l = Layout::new(PixelFormat::Rgba8, 10, 3);
        let mut dst = vec![0u8; l.staging_size as usize];
        assert!(l.fill(&f, &mut dst[..]));
        for y in 0..3 {
            assert_eq!(&dst[y * 256..y * 256 + 40], &f.data[y * 48..y * 48 + 40]);
        }
        let f = frame(PixelFormat::Nv12, 8, 4, 8);
        let l = Layout::new(PixelFormat::Nv12, 8, 4);
        let mut dst = vec![0u8; l.staging_size as usize];
        assert!(l.fill(&f, &mut dst[..]));
        let off = l.plane1_offset as usize;
        assert_eq!(&dst[off..off + 8], &f.data[32..40]);
        assert_eq!(&dst[off + 256..off + 264], &f.data[40..48]);
        let short = VideoFrame { data: vec![0; 10], ..frame(PixelFormat::Yuyv, 8, 4, 16) };
        assert!(!Layout::new(PixelFormat::Yuyv, 8, 4).fill(&short, &mut vec![0u8; 4096][..]));
    }
}
