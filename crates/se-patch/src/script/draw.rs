//! The Lua `draw.*` API (§6.4): Processing-style immediate calls appended to the patch's
//! pending [`DrawList`](se_hub::draw::DrawList), published after every `frame()`.
//!
//! Coordinates are normalized to the layer ((0,0) top-left, (1,1) bottom-right); sizes are
//! fractions of the layer height. Colors are `{r, g, b[, a]}` (0–1), `{r=, g=, b=, a=}`,
//! `"#rrggbb[aa]"`, or a palette entry; the optional `alpha` argument multiplies the color's
//! alpha (e.g. `env`).

use mlua::{FromLua, Lua, Table, Value as Lv};
use se_hub::draw::{DrawOp, Paint, PathCmd, TextAlign};
use std::cell::RefCell;
use std::rc::Rc;

/// Upper bound on ops per frame (a runaway loop must not exhaust memory).
pub const MAX_OPS: usize = 100_000;
/// Upper bound on a single text op (bytes).
const MAX_TEXT: usize = 4096;

#[derive(Default)]
pub struct DrawBuf {
    pub ops: Vec<DrawOp>,
    /// Open `push` scopes in the current frame.
    pub depth: u32,
}

impl DrawBuf {
    pub fn reset(&mut self) {
        self.ops.clear();
        self.depth = 0;
    }

    /// Close unbalanced `push` scopes so the renderer always sees a balanced list.
    pub fn finish(&mut self) {
        for _ in 0..self.depth {
            self.ops.push(DrawOp::Pop);
        }
        self.depth = 0;
    }

    fn push(&mut self, op: DrawOp) -> mlua::Result<()> {
        if self.ops.len() >= MAX_OPS {
            return Err(mlua::Error::runtime(format!("too many draw calls in one frame (max {MAX_OPS})")));
        }
        self.ops.push(op);
        Ok(())
    }
}

/// A color argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub [f32; 4]);

impl FromLua for Color {
    fn from_lua(v: Lv, _lua: &Lua) -> mlua::Result<Self> {
        let bad = |what: &str| mlua::Error::runtime(format!("bad color ({what}); use {{r,g,b[,a]}}, {{r=,g=,b=,a=}}, or \"#rrggbb[aa]\""));
        match v {
            Lv::Table(t) => {
                let r: Option<f64> = t.raw_get(1)?;
                let c = if let Some(r) = r {
                    [
                        r,
                        t.raw_get::<Option<f64>>(2)?.ok_or_else(|| bad("missing g"))?,
                        t.raw_get::<Option<f64>>(3)?.ok_or_else(|| bad("missing b"))?,
                        t.raw_get::<Option<f64>>(4)?.unwrap_or(1.0),
                    ]
                } else {
                    let g = |k: &str| t.raw_get::<Option<f64>>(k);
                    [
                        g("r")?.ok_or_else(|| bad("missing r"))?,
                        g("g")?.ok_or_else(|| bad("missing g"))?,
                        g("b")?.ok_or_else(|| bad("missing b"))?,
                        g("a")?.unwrap_or(1.0),
                    ]
                };
                Ok(Color(c.map(|x| x as f32)))
            }
            Lv::String(s) => se_proto::value::parse_hex_color(&s.to_string_lossy()).map(Color).ok_or_else(|| bad("hex")),
            Lv::Number(n) => Ok(Color([n as f32, n as f32, n as f32, 1.0])),
            Lv::Integer(n) => Ok(Color([n as f32, n as f32, n as f32, 1.0])),
            other => Err(bad(other.type_name())),
        }
    }
}

fn with_alpha(c: Color, alpha: Option<f64>) -> [f32; 4] {
    let mut c = c.0;
    if let Some(a) = alpha {
        c[3] *= a.clamp(0.0, 1.0) as f32;
    }
    c
}

fn finite(name: &str, xs: &[f64]) -> mlua::Result<()> {
    if xs.iter().all(|x| x.is_finite()) { Ok(()) } else { Err(mlua::Error::runtime(format!("draw.{name}: coordinates must be finite numbers"))) }
}

fn paint(opts: &Option<Table>) -> mlua::Result<Paint> {
    Ok(match opts {
        Some(o) => match o.raw_get::<Option<f64>>("stroke")? {
            Some(w) if w.is_finite() && w > 0.0 => Paint::Stroke(w as f32),
            _ => Paint::Fill,
        },
        None => Paint::Fill,
    })
}

fn opt_num(opts: &Option<Table>, key: &str) -> mlua::Result<Option<f64>> {
    match opts {
        Some(o) => o.raw_get::<Option<f64>>(key),
        None => Ok(None),
    }
}

fn path_cmds(t: &Table, close: bool) -> mlua::Result<Vec<PathCmd>> {
    let n = t.raw_len();
    let first: Lv = t.raw_get(1)?;
    let mut cmds = Vec::new();
    match first {
        // flat points {x1, y1, x2, y2, ...}
        Lv::Number(_) | Lv::Integer(_) => {
            if n < 4 || !n.is_multiple_of(2) {
                return Err(mlua::Error::runtime("draw.path: points must be {x1, y1, x2, y2, ...} (at least 2 points)"));
            }
            let mut pts = Vec::with_capacity(n / 2);
            for i in (1..=n).step_by(2) {
                let (x, y): (f64, f64) = (t.raw_get(i)?, t.raw_get(i + 1)?);
                finite("path", &[x, y])?;
                pts.push([x as f32, y as f32]);
            }
            cmds.push(PathCmd::MoveTo(pts[0]));
            cmds.extend(pts[1..].iter().map(|p| PathCmd::LineTo(*p)));
        }
        // commands {{"M", x, y}, {"L", x, y}, {"Q", cx, cy, x, y}, {"C", c1x, c1y, c2x, c2y, x, y}, {"Z"}}
        Lv::Table(_) => {
            for i in 1..=n {
                let c: Table = t.raw_get(i)?;
                let op: String = c.raw_get(1)?;
                let num = |k: usize| -> mlua::Result<f32> {
                    let v: f64 = c.raw_get(k)?;
                    finite("path", &[v])?;
                    Ok(v as f32)
                };
                cmds.push(match op.as_str() {
                    "M" | "m" => PathCmd::MoveTo([num(2)?, num(3)?]),
                    "L" | "l" => PathCmd::LineTo([num(2)?, num(3)?]),
                    "Q" | "q" => PathCmd::QuadTo([num(2)?, num(3)?], [num(4)?, num(5)?]),
                    "C" | "c" => PathCmd::CubicTo([num(2)?, num(3)?], [num(4)?, num(5)?], [num(6)?, num(7)?]),
                    "Z" | "z" => PathCmd::Close,
                    other => return Err(mlua::Error::runtime(format!("draw.path: unknown command `{other}` (M, L, Q, C, Z)"))),
                });
            }
            if !matches!(cmds.first(), Some(PathCmd::MoveTo(_))) {
                return Err(mlua::Error::runtime("draw.path: must start with {\"M\", x, y}"));
            }
        }
        _ => return Err(mlua::Error::runtime("draw.path: expected a table of points or commands")),
    }
    if close && !matches!(cmds.last(), Some(PathCmd::Close)) {
        cmds.push(PathCmd::Close);
    }
    Ok(cmds)
}

fn safe_asset_path(p: &str) -> mlua::Result<String> {
    let p = p.trim_start_matches("assets/");
    if p.is_empty() || p.starts_with('/') || p.split(['/', '\\']).any(|s| s == "..") {
        return Err(mlua::Error::runtime(format!("draw.image: `{p}` must be a path inside the project assets/ folder")));
    }
    Ok(p.to_string())
}

/// Build the `draw` table bound to `buf`.
pub fn install(lua: &Lua, buf: Rc<RefCell<DrawBuf>>) -> mlua::Result<Table> {
    let draw = lua.create_table()?;

    let b = buf.clone();
    draw.set(
        "clear",
        lua.create_function(move |_, color: Option<Color>| {
            let mut b = b.borrow_mut();
            // everything drawn before a top-level clear is invisible: drop it
            if b.depth == 0 {
                b.ops.clear();
            }
            b.push(DrawOp::Clear(color.map(|c| c.0).unwrap_or([0.0; 4])))
        })?,
    )?;

    let b = buf.clone();
    draw.set(
        "rect",
        lua.create_function(move |_, (x, y, w, h, color, alpha, opts): (f64, f64, f64, f64, Color, Option<f64>, Option<Table>)| {
            finite("rect", &[x, y, w, h])?;
            let radius = opt_num(&opts, "radius")?.unwrap_or(0.0).max(0.0) as f32;
            let paint = paint(&opts)?;
            b.borrow_mut().push(DrawOp::Rect { xywh: [x as f32, y as f32, w as f32, h as f32], radius, color: with_alpha(color, alpha), paint })
        })?,
    )?;

    let b = buf.clone();
    draw.set(
        "circle",
        lua.create_function(move |_, (x, y, r, color, alpha, opts): (f64, f64, f64, Color, Option<f64>, Option<Table>)| {
            finite("circle", &[x, y, r])?;
            let paint = paint(&opts)?;
            b.borrow_mut().push(DrawOp::Circle { center: [x as f32, y as f32], radius: r.max(0.0) as f32, color: with_alpha(color, alpha), paint })
        })?,
    )?;

    let b = buf.clone();
    draw.set(
        "line",
        lua.create_function(move |_, (x1, y1, x2, y2, width, color, alpha): (f64, f64, f64, f64, f64, Color, Option<f64>)| {
            finite("line", &[x1, y1, x2, y2, width])?;
            b.borrow_mut().push(DrawOp::Line {
                a: [x1 as f32, y1 as f32],
                b: [x2 as f32, y2 as f32],
                width: width.max(0.0) as f32,
                color: with_alpha(color, alpha),
            })
        })?,
    )?;

    let b = buf.clone();
    draw.set(
        "path",
        lua.create_function(move |_, (pts, color, alpha, opts): (Table, Color, Option<f64>, Option<Table>)| {
            let close = match &opts {
                Some(o) => o.raw_get::<Option<bool>>("close")?.unwrap_or(false),
                None => false,
            };
            let cmds = path_cmds(&pts, close)?;
            let paint = paint(&opts)?;
            b.borrow_mut().push(DrawOp::Path { cmds, color: with_alpha(color, alpha), paint })
        })?,
    )?;

    let b = buf.clone();
    draw.set(
        "text",
        lua.create_function(move |_, (text, x, y, size, color, alpha, opts): (mlua::LuaString, f64, f64, f64, Color, Option<f64>, Option<Table>)| {
            finite("text", &[x, y, size])?;
            let mut text = text.to_string_lossy();
            if text.len() > MAX_TEXT {
                let mut cut = MAX_TEXT;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
            }
            let align = match &opts {
                Some(o) => match o.raw_get::<Option<String>>("align")?.as_deref() {
                    None | Some("left") => TextAlign::Left,
                    Some("center") => TextAlign::Center,
                    Some("right") => TextAlign::Right,
                    Some(other) => return Err(mlua::Error::runtime(format!("draw.text: align must be left, center, or right (got `{other}`)"))),
                },
                None => TextAlign::Left,
            };
            b.borrow_mut().push(DrawOp::Text { pos: [x as f32, y as f32], size: size.max(0.0) as f32, color: with_alpha(color, alpha), text, align })
        })?,
    )?;

    let b = buf.clone();
    draw.set(
        "image",
        lua.create_function(move |_, (path, x, y, w, h, alpha): (String, f64, f64, f64, f64, Option<f64>)| {
            finite("image", &[x, y, w, h])?;
            let path = safe_asset_path(&path)?;
            b.borrow_mut().push(DrawOp::Image { path, xywh: [x as f32, y as f32, w as f32, h as f32], opacity: alpha.unwrap_or(1.0).clamp(0.0, 1.0) as f32 })
        })?,
    )?;

    let b = buf.clone();
    draw.set(
        "push",
        lua.create_function(move |_, opts: Option<Table>| {
            let (mut translate, mut rotate, mut scale, mut alpha) = ([0.0f32; 2], 0.0f32, [1.0f32; 2], 1.0f32);
            if let Some(o) = &opts {
                translate = [o.raw_get::<Option<f64>>("x")?.unwrap_or(0.0) as f32, o.raw_get::<Option<f64>>("y")?.unwrap_or(0.0) as f32];
                rotate = o.raw_get::<Option<f64>>("rotate")?.unwrap_or(0.0) as f32;
                scale = match o.raw_get::<Lv>("scale")? {
                    Lv::Number(s) => [s as f32; 2],
                    Lv::Integer(s) => [s as f32; 2],
                    Lv::Table(t) => [t.raw_get::<f64>(1)? as f32, t.raw_get::<f64>(2)? as f32],
                    Lv::Nil => [1.0; 2],
                    other => return Err(mlua::Error::runtime(format!("draw.push: scale must be a number or {{sx, sy}} (got {})", other.type_name()))),
                };
                alpha = o.raw_get::<Option<f64>>("alpha")?.unwrap_or(1.0).clamp(0.0, 1.0) as f32;
            }
            if ![translate[0], translate[1], rotate, scale[0], scale[1]].iter().all(|v| v.is_finite()) {
                return Err(mlua::Error::runtime("draw.push: values must be finite numbers"));
            }
            let mut b = b.borrow_mut();
            if b.depth >= 64 {
                return Err(mlua::Error::runtime("draw.push: nested too deeply (max 64)"));
            }
            b.push(DrawOp::Push { translate, rotate, scale, alpha })?;
            b.depth += 1;
            Ok(())
        })?,
    )?;

    let b = buf;
    draw.set(
        "pop",
        lua.create_function(move |_, ()| {
            let mut b = b.borrow_mut();
            if b.depth == 0 {
                return Err(mlua::Error::runtime("draw.pop without a matching draw.push"));
            }
            b.depth -= 1;
            b.push(DrawOp::Pop)
        })?,
    )?;

    Ok(draw)
}
