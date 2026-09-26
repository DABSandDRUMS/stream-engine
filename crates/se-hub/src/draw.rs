//! 2D draw lists produced by script patches (Lua `draw.*`) and rendered on the GPU by
//! `se-render` (vello). Coordinates are normalized to the layer: (0,0) top-left, (1,1)
//! bottom-right; sizes in the same units (radius 0.01 = 1% of the layer height).
//! Colors are linear-ish sRGB RGBA 0–1.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, PartialEq)]
pub enum PathCmd {
    MoveTo([f32; 2]),
    LineTo([f32; 2]),
    QuadTo([f32; 2], [f32; 2]),
    CubicTo([f32; 2], [f32; 2], [f32; 2]),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    Fill,
    /// Stroke width in normalized units.
    Stroke(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum DrawOp {
    /// Clear the layer to a color (transparent by default).
    Clear([f32; 4]),
    Rect {
        xywh: [f32; 4],
        radius: f32,
        color: [f32; 4],
        paint: Paint,
    },
    Circle {
        center: [f32; 2],
        radius: f32,
        color: [f32; 4],
        paint: Paint,
    },
    Line {
        a: [f32; 2],
        b: [f32; 2],
        width: f32,
        color: [f32; 4],
    },
    Path {
        cmds: Vec<PathCmd>,
        color: [f32; 4],
        paint: Paint,
    },
    /// Text at a baseline-left position; `size` is the em height (normalized).
    Text {
        pos: [f32; 2],
        size: f32,
        color: [f32; 4],
        text: String,
        align: TextAlign,
    },
    /// Image from the project `assets/` (path relative to it), drawn into `xywh`.
    Image {
        path: String,
        xywh: [f32; 4],
        opacity: f32,
    },
    /// Push/pop a transform (translate, rotate radians, scale) and global alpha.
    Push {
        translate: [f32; 2],
        rotate: f32,
        scale: [f32; 2],
        alpha: f32,
    },
    Pop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub ops: Vec<DrawOp>,
    /// Increments per published list.
    pub seq: u64,
}

pub struct DrawWriter {
    input: triple_buffer::Input<DrawList>,
    seq: u64,
}

impl DrawWriter {
    /// Build the next list in the reused back buffer and publish it.
    pub fn publish_with(&mut self, build: impl FnOnce(&mut Vec<DrawOp>)) {
        let b = self.input.input_buffer_mut();
        b.ops.clear();
        build(&mut b.ops);
        self.seq += 1;
        b.seq = self.seq;
        self.input.publish();
    }
}

pub struct DrawReader {
    output: triple_buffer::Output<DrawList>,
}

impl DrawReader {
    /// Newest list (may be the same as last time; compare `seq`).
    pub fn latest(&mut self) -> &DrawList {
        self.output.update();
        self.output.output_buffer()
    }
}

#[derive(Default)]
pub struct DrawSlots {
    readers: Mutex<HashMap<String, DrawReader>>,
    pub generation: AtomicU64,
}

impl DrawSlots {
    pub fn register(&self, name: &str) -> DrawWriter {
        let (input, output) = triple_buffer::TripleBuffer::new(&DrawList::default()).split();
        self.readers.lock().insert(name.to_string(), DrawReader { output });
        self.generation.fetch_add(1, Ordering::Release);
        DrawWriter { input, seq: 0 }
    }

    pub fn take_new(&self) -> Vec<(String, DrawReader)> {
        self.readers.lock().drain().collect()
    }
}
