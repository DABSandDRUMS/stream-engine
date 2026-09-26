//! Adobe/Resolve `.cube` 3D LUTs (§4.2 color): parsed off the render thread, uploaded as an
//! `Rgba16Float` 3D texture sampled with trilinear filtering.

use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Lut {
    pub size: u32,
    /// RGB triples, red fastest (cube order).
    pub data: Vec<[f32; 3]>,
}

impl Lut {
    pub fn identity(size: u32) -> Lut {
        let n = size.max(2);
        let mut data = Vec::with_capacity((n * n * n) as usize);
        let f = |i: u32| i as f32 / (n - 1) as f32;
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    data.push([f(r), f(g), f(b)]);
                }
            }
        }
        Lut { size: n, data }
    }

    pub fn parse(src: &str) -> Result<Lut, String> {
        let mut size = None;
        let mut domain_min = [0.0f32; 3];
        let mut domain_max = [1.0f32; 3];
        let mut data = Vec::new();
        for (ln, line) in src.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let head = it.next().unwrap_or("");
            let nums = |it: std::str::SplitWhitespace| -> Result<[f32; 3], String> {
                let v: Vec<f32> = it.map(|x| x.parse::<f32>().map_err(|_| format!("line {}: bad number `{x}`", ln + 1))).collect::<Result<_, _>>()?;
                if v.len() != 3 {
                    return Err(format!("line {}: expected 3 numbers", ln + 1));
                }
                Ok([v[0], v[1], v[2]])
            };
            match head {
                "TITLE" => {}
                "LUT_3D_SIZE" => {
                    let n: u32 = it.next().and_then(|x| x.parse().ok()).ok_or_else(|| format!("line {}: bad LUT_3D_SIZE", ln + 1))?;
                    if !(2..=256).contains(&n) {
                        return Err(format!("line {}: LUT_3D_SIZE {n} out of range (2–256)", ln + 1));
                    }
                    size = Some(n);
                }
                "LUT_1D_SIZE" => return Err("1D LUTs are not supported (use a 3D .cube)".into()),
                "DOMAIN_MIN" => domain_min = nums(it)?,
                "DOMAIN_MAX" => domain_max = nums(it)?,
                "LUT_3D_INPUT_RANGE" => {
                    let v: Vec<f32> = it.filter_map(|x| x.parse().ok()).collect();
                    if v.len() != 2 {
                        return Err(format!("line {}: LUT_3D_INPUT_RANGE needs min and max", ln + 1));
                    }
                    domain_min = [v[0]; 3];
                    domain_max = [v[1]; 3];
                }
                h if h.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '.') => {
                    let v = nums(line.split_whitespace())?;
                    data.push(v);
                }
                _ => {} // unknown keywords are ignored per the spec
            }
        }
        let n = size.ok_or("missing LUT_3D_SIZE")?;
        if data.len() != (n * n * n) as usize {
            return Err(format!("expected {} entries, found {}", n * n * n, data.len()));
        }
        if domain_min != [0.0; 3] || domain_max != [1.0; 3] {
            for d in &mut data {
                for c in 0..3 {
                    let span = (domain_max[c] - domain_min[c]).max(1e-6);
                    d[c] = (d[c] - domain_min[c]) / span;
                }
            }
        }
        Ok(Lut { size: n, data })
    }

    pub fn load(path: &Path) -> Result<Lut, String> {
        let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Lut::parse(&src).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Texel data for an Rgba16Float 3D texture.
    pub fn rgba16f(&self) -> Vec<u16> {
        let mut out = Vec::with_capacity(self.data.len() * 4);
        for d in &self.data {
            out.extend_from_slice(&[f16(d[0]), f16(d[1]), f16(d[2]), f16(1.0)]);
        }
        out
    }
}

/// f32 → IEEE half (round to nearest even), enough range for LUT values.
pub fn f16(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let mant = x & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = (mant | 0x80_0000) >> (1 - e);
        let round = (m >> 12) & 1;
        return sign | ((m >> 13) + round) as u16;
    }
    let m = mant >> 13;
    let round_bits = mant & 0x1fff;
    let mut h = sign as u32 | ((e as u32) << 10) | m;
    if round_bits > 0x1000 || (round_bits == 0x1000 && (m & 1) == 1) {
        h += 1;
    }
    h as u16
}

pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, lut: &Lut) -> wgpu::TextureView {
    let n = lut.size;
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lut"),
        size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let data = lut.rgba16f();
    queue.write_texture(
        tex.as_image_copy(),
        bytemuck::cast_slice(&data),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 8), rows_per_image: Some(n) },
        wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
    );
    tex.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D3), ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cube_with_domain_and_comments() {
        let mut s = String::from("# comment\nTITLE \"x\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 2 2 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    s += &format!("{} {} {}\n", r * 2, g * 2, b * 2);
                }
            }
        }
        let l = Lut::parse(&s).unwrap();
        assert_eq!(l, Lut::identity(2));
    }

    #[test]
    fn rejects_bad_files() {
        assert!(Lut::parse("LUT_3D_SIZE 2\n0 0 0\n").unwrap_err().contains("expected 8"));
        assert!(Lut::parse("0 0 0\n").unwrap_err().contains("LUT_3D_SIZE"));
        assert!(Lut::parse("LUT_1D_SIZE 16\n").is_err());
        assert!(Lut::parse("LUT_3D_SIZE 2\n0 0 x\n").unwrap_err().contains("bad number"));
        assert!(Lut::parse("LUT_3D_SIZE 999\n").is_err());
    }

    #[test]
    fn half_floats() {
        assert_eq!(f16(0.0), 0);
        assert_eq!(f16(1.0), 0x3c00);
        assert_eq!(f16(0.5), 0x3800);
        assert_eq!(f16(-2.0), 0xc000);
        assert_eq!(f16(65504.0), 0x7bff);
        assert_eq!(f16(1e9), 0x7c00);
        assert_eq!(f16(0.333_333_34), 0x3555);
    }
}
