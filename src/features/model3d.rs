//! Built-in 3D model overview: pure Rust, no Blender and no external viewer.
//!
//! Loads STL, OBJ, PLY, OFF, glTF/GLB and 3MF into a triangle soup and draws
//! it with a small software rasterizer (z-buffer, flat shading, 2x
//! supersampling). Blender `.blend` files show the thumbnail Blender saved
//! inside the file; Spotty does not parse the scene itself.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Models bigger than this are not read (the preview must stay quick).
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Above this many triangles the renderer draws every n-th one.
const MAX_DRAWN_TRIS: usize = 3_000_000;
const SUPERSAMPLE: usize = 2;

pub(crate) const MODEL_EXTS: &[&str] = &[
    "stl", "obj", "ply", "off", "glb", "gltf", "3mf", "blend", "blend1",
];

pub(crate) fn is_model_ext(ext: &str) -> bool {
    MODEL_EXTS.contains(&ext)
}

pub(crate) struct Mesh {
    /// Three consecutive points per triangle.
    pts: Vec<[f32; 3]>,
    /// True when +Z is up (STL, PLY, 3MF, OFF); false for Y-up (OBJ, glTF).
    z_up: bool,
    format: &'static str,
}

impl Mesh {
    fn tri_count(&self) -> usize {
        self.pts.len() / 3
    }

    fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        let first = *self.pts.first()?;
        let (mut lo, mut hi) = (first, first);
        for p in &self.pts {
            for i in 0..3 {
                lo[i] = lo[i].min(p[i]);
                hi[i] = hi[i].max(p[i]);
            }
        }
        Some((lo, hi))
    }

    fn caption(&self) -> String {
        let mut s = format!("{} \u{b7} {} triangles", self.format, group_digits(self.tri_count()));
        if let Some((lo, hi)) = self.bounds() {
            s.push_str(&format!(
                " \u{b7} {} \u{d7} {} \u{d7} {}",
                short_num(hi[0] - lo[0]),
                short_num(hi[1] - lo[1]),
                short_num(hi[2] - lo[2])
            ));
        }
        s
    }
}

fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn short_num(v: f32) -> String {
    if !v.is_finite() {
        return "?".into();
    }
    let a = v.abs();
    if a >= 100.0 {
        format!("{:.0}", v)
    } else if a >= 10.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.2}", v)
    }
}

// ── Public entry points ─────────────────────────────────────────────

static CAPTIONS: OnceLock<Mutex<HashMap<std::path::PathBuf, String>>> = OnceLock::new();

/// Caption stored by the last overview of `path` (format, triangles, size).
pub(crate) fn caption_for(path: &Path) -> Option<String> {
    CAPTIONS.get()?.lock().ok()?.get(path).cloned()
}

fn store_caption(path: &Path, caption: String) {
    if let Ok(mut map) = CAPTIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        if map.len() > 256 {
            map.clear();
        }
        map.insert(path.to_path_buf(), caption);
    }
}

/// Render an overview of the model at `path` into a `w`×`h` RGBA image that
/// fits inside the box (the result may be smaller on one side). Works on a
/// worker thread; touches no GTK state.
pub(crate) fn overview(path: &Path, max_w: u32, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())?
        .to_ascii_lowercase();
    if ext == "blend" || ext == "blend1" {
        let (w, h, rgba) = blend_thumbnail(path)?;
        store_caption(path, format!("Blender file \u{b7} saved thumbnail ({w}\u{d7}{h})"));
        return Some(fit_rgba(rgba, w, h, max_w, max_h));
    }
    let mesh = load_mesh(path, &ext)?;
    if mesh.tri_count() == 0 {
        return None;
    }
    store_caption(path, mesh.caption());
    Some((render(&mesh, max_w as usize, max_h as usize), max_w, max_h))
}

fn fit_rgba(rgba: Vec<u8>, w: u32, h: u32, max_w: u32, max_h: u32) -> (Vec<u8>, u32, u32) {
    if w <= max_w && h <= max_h {
        return (rgba, w, h);
    }
    let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else {
        return (Vec::new(), 0, 0);
    };
    let scale = (max_w as f32 / w as f32).min(max_h as f32 / h as f32);
    let nw = ((w as f32 * scale) as u32).max(1);
    let nh = ((h as f32 * scale) as u32).max(1);
    let out = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3);
    (out.into_raw(), nw, nh)
}

fn read_capped(path: &Path) -> Option<Vec<u8>> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

fn load_mesh(path: &Path, ext: &str) -> Option<Mesh> {
    match ext {
        "stl" => parse_stl(&read_capped(path)?),
        "obj" => parse_obj(&read_capped(path)?),
        "ply" => parse_ply(&read_capped(path)?),
        "off" => parse_off(&read_capped(path)?),
        "glb" => parse_gltf(&read_capped(path)?, path),
        "gltf" => parse_gltf(&read_capped(path)?, path),
        "3mf" => parse_3mf(path),
        _ => None,
    }
}

// ── STL ─────────────────────────────────────────────────────────────

fn f32_le(b: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn parse_stl(data: &[u8]) -> Option<Mesh> {
    if data.len() >= 84 {
        let n = u32::from_le_bytes([data[80], data[81], data[82], data[83]]) as usize;
        if n.checked_mul(50)?.checked_add(84)? == data.len() {
            let mut pts = Vec::with_capacity(n * 3);
            for t in 0..n {
                let base = 84 + t * 50 + 12;
                for v in 0..3 {
                    let o = base + v * 12;
                    pts.push([f32_le(data, o), f32_le(data, o + 4), f32_le(data, o + 8)]);
                }
            }
            return Some(Mesh { pts, z_up: true, format: "STL" });
        }
    }
    let text = String::from_utf8_lossy(data);
    let mut pts = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if it.next() == Some("vertex") {
            let v: Vec<f32> = it.filter_map(|t| t.parse().ok()).collect();
            if v.len() >= 3 {
                pts.push([v[0], v[1], v[2]]);
            }
        }
    }
    pts.truncate(pts.len() / 3 * 3);
    (!pts.is_empty()).then_some(Mesh { pts, z_up: true, format: "STL" })
}

// ── OBJ ─────────────────────────────────────────────────────────────

fn parse_obj(data: &[u8]) -> Option<Mesh> {
    let text = String::from_utf8_lossy(data);
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut pts = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("v ") {
            let v: Vec<f32> = rest
                .split_whitespace()
                .take(3)
                .filter_map(|t| t.parse().ok())
                .collect();
            if v.len() == 3 {
                verts.push([v[0], v[1], v[2]]);
            } else {
                verts.push([0.0; 3]);
            }
        } else if let Some(rest) = line.strip_prefix("f ") {
            let idx: Vec<usize> = rest
                .split_whitespace()
                .filter_map(|tok| {
                    let n: i64 = tok.split('/').next()?.parse().ok()?;
                    let i = if n < 0 { verts.len() as i64 + n } else { n - 1 };
                    (i >= 0 && (i as usize) < verts.len()).then_some(i as usize)
                })
                .collect();
            for k in 1..idx.len().saturating_sub(1) {
                pts.push(verts[idx[0]]);
                pts.push(verts[idx[k]]);
                pts.push(verts[idx[k + 1]]);
            }
        }
    }
    (!pts.is_empty()).then_some(Mesh { pts, z_up: false, format: "OBJ" })
}

// ── PLY ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum PlyType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

fn ply_type(name: &str) -> Option<PlyType> {
    Some(match name {
        "char" | "int8" => PlyType::I8,
        "uchar" | "uint8" => PlyType::U8,
        "short" | "int16" => PlyType::I16,
        "ushort" | "uint16" => PlyType::U16,
        "int" | "int32" => PlyType::I32,
        "uint" | "uint32" => PlyType::U32,
        "float" | "float32" => PlyType::F32,
        "double" | "float64" => PlyType::F64,
        _ => return None,
    })
}

struct PlyProp {
    name: String,
    /// Some(count type) for a list property.
    list: Option<PlyType>,
    ty: PlyType,
}

struct PlyElement {
    name: String,
    count: usize,
    props: Vec<PlyProp>,
}

struct PlyReader<'a> {
    data: &'a [u8],
    pos: usize,
    ascii: bool,
    big: bool,
}

impl PlyReader<'_> {
    fn bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        let s = self.data.get(self.pos..self.pos + N)?;
        self.pos += N;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        if self.big {
            a.reverse();
        }
        Some(a)
    }

    fn num(&mut self, ty: PlyType) -> Option<f64> {
        if self.ascii {
            while self.data.get(self.pos)?.is_ascii_whitespace() {
                self.pos += 1;
            }
            let start = self.pos;
            while self.pos < self.data.len() && !self.data[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            return std::str::from_utf8(&self.data[start..self.pos]).ok()?.parse().ok();
        }
        Some(match ty {
            PlyType::I8 => i8::from_le_bytes(self.bytes::<1>()?) as f64,
            PlyType::U8 => self.bytes::<1>()?[0] as f64,
            PlyType::I16 => i16::from_le_bytes(self.bytes::<2>()?) as f64,
            PlyType::U16 => u16::from_le_bytes(self.bytes::<2>()?) as f64,
            PlyType::I32 => i32::from_le_bytes(self.bytes::<4>()?) as f64,
            PlyType::U32 => u32::from_le_bytes(self.bytes::<4>()?) as f64,
            PlyType::F32 => f32::from_le_bytes(self.bytes::<4>()?) as f64,
            PlyType::F64 => f64::from_le_bytes(self.bytes::<8>()?),
        })
    }
}

fn parse_ply(data: &[u8]) -> Option<Mesh> {
    let end = data.windows(10).position(|w| w == b"end_header")?;
    let body = end + data[end..].iter().position(|b| *b == b'\n')? + 1;
    let header = String::from_utf8_lossy(&data[..end]);
    let mut format = "ascii";
    let mut elements: Vec<PlyElement> = Vec::new();
    for line in header.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        match t.as_slice() {
            ["format", f, ..] => format = f,
            ["element", name, count] => elements.push(PlyElement {
                name: name.to_string(),
                count: count.parse().ok()?,
                props: Vec::new(),
            }),
            ["property", "list", cty, ity, name] => {
                let el = elements.last_mut()?;
                el.props.push(PlyProp {
                    name: name.to_string(),
                    list: Some(ply_type(cty)?),
                    ty: ply_type(ity)?,
                });
            }
            ["property", ty, name] => {
                let el = elements.last_mut()?;
                el.props.push(PlyProp {
                    name: name.to_string(),
                    list: None,
                    ty: ply_type(ty)?,
                });
            }
            _ => {}
        }
    }
    let mut r = PlyReader {
        data,
        pos: body,
        ascii: format == "ascii",
        big: format == "binary_big_endian",
    };
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut pts = Vec::new();
    for el in &elements {
        for _ in 0..el.count {
            let mut xyz = [0.0f32; 3];
            let mut face: Vec<usize> = Vec::new();
            for p in &el.props {
                if let Some(cty) = p.list {
                    let n = r.num(cty)? as usize;
                    let is_face = el.name == "face" && (p.name == "vertex_indices" || p.name == "vertex_index");
                    for _ in 0..n {
                        let v = r.num(p.ty)?;
                        if is_face {
                            face.push(v as usize);
                        }
                    }
                } else {
                    let v = r.num(p.ty)? as f32;
                    if el.name == "vertex" {
                        match p.name.as_str() {
                            "x" => xyz[0] = v,
                            "y" => xyz[1] = v,
                            "z" => xyz[2] = v,
                            _ => {}
                        }
                    }
                }
            }
            if el.name == "vertex" {
                verts.push(xyz);
            } else if el.name == "face" && face.iter().all(|i| *i < verts.len()) {
                for k in 1..face.len().saturating_sub(1) {
                    pts.push(verts[face[0]]);
                    pts.push(verts[face[k]]);
                    pts.push(verts[face[k + 1]]);
                }
            }
        }
    }
    (!pts.is_empty()).then_some(Mesh { pts, z_up: true, format: "PLY" })
}

// ── OFF ─────────────────────────────────────────────────────────────

fn parse_off(data: &[u8]) -> Option<Mesh> {
    let text = String::from_utf8_lossy(data);
    let mut tokens = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .flat_map(|l| l.split_whitespace());
    let magic = tokens.next()?;
    if !magic.ends_with("OFF") {
        return None;
    }
    let nv: usize = tokens.next()?.parse().ok()?;
    let nf: usize = tokens.next()?.parse().ok()?;
    let _edges = tokens.next()?;
    let mut verts = Vec::with_capacity(nv.min(1 << 24));
    for _ in 0..nv {
        let x: f32 = tokens.next()?.parse().ok()?;
        let y: f32 = tokens.next()?.parse().ok()?;
        let z: f32 = tokens.next()?.parse().ok()?;
        verts.push([x, y, z]);
    }
    let mut pts = Vec::new();
    for _ in 0..nf {
        let n: usize = tokens.next()?.parse().ok()?;
        let mut idx = Vec::with_capacity(n);
        for _ in 0..n {
            let i: usize = tokens.next()?.parse().ok()?;
            idx.push(*verts.get(i)?);
        }
        for k in 1..n.saturating_sub(1) {
            pts.extend([idx[0], idx[k], idx[k + 1]]);
        }
    }
    (!pts.is_empty()).then_some(Mesh { pts, z_up: true, format: "OFF" })
}

// ── glTF / GLB ──────────────────────────────────────────────────────

type Mat4 = [f32; 16]; // column-major, as in glTF

const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut o = [0.0; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[c * 4 + r] = (0..4).map(|k| a[k * 4 + r] * b[c * 4 + k]).sum();
        }
    }
    o
}

fn mat_apply(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

struct Gltf<'a> {
    json: &'a serde_json::Value,
    buffers: Vec<Vec<u8>>,
}

impl Gltf<'_> {
    /// Read an accessor as `n` floats per element (positions) or indices.
    fn accessor(&self, idx: usize) -> Option<(Vec<f64>, usize)> {
        let acc = self.json.get("accessors")?.get(idx)?;
        let count = acc.get("count")?.as_u64()? as usize;
        let comps = match acc.get("type")?.as_str()? {
            "SCALAR" => 1,
            "VEC2" => 2,
            "VEC3" => 3,
            "VEC4" => 4,
            _ => return None,
        };
        let ctype = acc.get("componentType")?.as_u64()?;
        let size = match ctype {
            5120 | 5121 => 1,
            5122 | 5123 => 2,
            5125 | 5126 => 4,
            _ => return None,
        };
        let mut out = vec![0.0; count * comps];
        let Some(view_idx) = acc.get("bufferView").and_then(|v| v.as_u64()) else {
            return Some((out, comps));
        };
        let view = self.json.get("bufferViews")?.get(view_idx as usize)?;
        let buf = self.buffers.get(view.get("buffer")?.as_u64()? as usize)?;
        let base = view.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize
            + acc.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let stride = view
            .get("byteStride")
            .and_then(|v| v.as_u64())
            .map(|s| s as usize)
            .unwrap_or(size * comps);
        for i in 0..count {
            for c in 0..comps {
                let o = base + i * stride + c * size;
                let b = buf.get(o..o + size)?;
                out[i * comps + c] = match ctype {
                    5120 => b[0] as i8 as f64,
                    5121 => b[0] as f64,
                    5122 => i16::from_le_bytes([b[0], b[1]]) as f64,
                    5123 => u16::from_le_bytes([b[0], b[1]]) as f64,
                    5125 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
                    _ => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
                };
            }
        }
        Some((out, comps))
    }

    fn node_matrix(node: &serde_json::Value) -> Mat4 {
        let nums = |key: &str, n: usize| -> Option<Vec<f32>> {
            let a = node.get(key)?.as_array()?;
            (a.len() == n).then(|| a.iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect())
        };
        if let Some(m) = nums("matrix", 16).filter(|m| m.len() == 16) {
            let mut o = [0.0; 16];
            o.copy_from_slice(&m);
            return o;
        }
        let t = nums("translation", 3).filter(|v| v.len() == 3).unwrap_or(vec![0.0; 3]);
        let s = nums("scale", 3).filter(|v| v.len() == 3).unwrap_or(vec![1.0; 3]);
        let q = nums("rotation", 4).filter(|v| v.len() == 4).unwrap_or(vec![0.0, 0.0, 0.0, 1.0]);
        let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
        let r = [
            1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y + z * w), 2.0 * (x * z - y * w),
            2.0 * (x * y - z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z + x * w),
            2.0 * (x * z + y * w), 2.0 * (y * z - x * w), 1.0 - 2.0 * (x * x + y * y),
        ];
        [
            r[0] * s[0], r[1] * s[0], r[2] * s[0], 0.0,
            r[3] * s[1], r[4] * s[1], r[5] * s[1], 0.0,
            r[6] * s[2], r[7] * s[2], r[8] * s[2], 0.0,
            t[0], t[1], t[2], 1.0,
        ]
    }

    fn add_mesh(&self, mesh_idx: usize, m: &Mat4, pts: &mut Vec<[f32; 3]>) -> Option<()> {
        let mesh = self.json.get("meshes")?.get(mesh_idx)?;
        for prim in mesh.get("primitives")?.as_array()? {
            let mode = prim.get("mode").and_then(|v| v.as_u64()).unwrap_or(4);
            let Some(pos_idx) = prim
                .get("attributes")
                .and_then(|a| a.get("POSITION"))
                .and_then(|v| v.as_u64())
            else {
                continue;
            };
            let Some((pos, 3)) = self.accessor(pos_idx as usize) else {
                continue;
            };
            let verts: Vec<[f32; 3]> = pos
                .chunks(3)
                .map(|c| mat_apply(m, [c[0] as f32, c[1] as f32, c[2] as f32]))
                .collect();
            let indices: Vec<usize> = match prim.get("indices").and_then(|v| v.as_u64()) {
                Some(i) => match self.accessor(i as usize) {
                    Some((v, _)) => v.into_iter().map(|f| f as usize).collect(),
                    None => continue,
                },
                None => (0..verts.len()).collect(),
            };
            let mut tri = |a: usize, b: usize, c: usize| {
                if let (Some(a), Some(b), Some(c)) = (verts.get(a), verts.get(b), verts.get(c)) {
                    pts.extend([*a, *b, *c]);
                }
            };
            match mode {
                4 => indices.chunks_exact(3).for_each(|t| tri(t[0], t[1], t[2])),
                5 => (2..indices.len()).for_each(|i| {
                    if i % 2 == 0 {
                        tri(indices[i - 2], indices[i - 1], indices[i])
                    } else {
                        tri(indices[i - 1], indices[i - 2], indices[i])
                    }
                }),
                6 => (2..indices.len()).for_each(|i| tri(indices[0], indices[i - 1], indices[i])),
                _ => {}
            }
        }
        Some(())
    }

    fn walk(&self, node_idx: usize, parent: &Mat4, depth: usize, pts: &mut Vec<[f32; 3]>) {
        if depth > 64 {
            return;
        }
        let Some(node) = self.json.get("nodes").and_then(|n| n.get(node_idx)) else {
            return;
        };
        let m = mat_mul(parent, &Self::node_matrix(node));
        if let Some(mesh) = node.get("mesh").and_then(|v| v.as_u64()) {
            let _ = self.add_mesh(mesh as usize, &m, pts);
        }
        if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
            for c in children.iter().filter_map(|c| c.as_u64()) {
                self.walk(c as usize, &m, depth + 1, pts);
            }
        }
    }
}

fn parse_gltf(data: &[u8], path: &Path) -> Option<Mesh> {
    let (json_bytes, glb_bin): (&[u8], Option<&[u8]>) = if data.starts_with(b"glTF") {
        let mut pos = 12;
        let (mut json, mut bin) = (None, None);
        while pos + 8 <= data.len() {
            let len = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
            let kind = &data[pos + 4..pos + 8];
            let chunk = data.get(pos + 8..pos + 8 + len)?;
            match kind {
                b"JSON" => json = Some(chunk),
                b"BIN\0" => bin = Some(chunk),
                _ => {}
            }
            pos += 8 + len.div_ceil(4) * 4;
        }
        (json?, bin)
    } else {
        (data, None)
    };
    let json: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
    let dir = path.parent()?;
    let mut buffers = Vec::new();
    for b in json.get("buffers").and_then(|b| b.as_array()).into_iter().flatten() {
        let bytes = match b.get("uri").and_then(|u| u.as_str()) {
            Some(uri) if uri.starts_with("data:") => {
                base64_decode(uri.split_once(",")?.1)?
            }
            Some(uri) => {
                // Sibling files only: never follow a path out of the model's folder.
                let rel = urlencoding::decode(uri).ok()?.into_owned();
                if rel.contains("..") || rel.starts_with('/') {
                    Vec::new()
                } else {
                    read_capped(&dir.join(rel)).unwrap_or_default()
                }
            }
            None => glb_bin.map(|b| b.to_vec()).unwrap_or_default(),
        };
        buffers.push(bytes);
    }
    let g = Gltf { json: &json, buffers };
    let mut pts = Vec::new();
    let nodes = json.get("nodes").and_then(|n| n.as_array());
    let scene_roots: Vec<usize> = json
        .get("scenes")
        .and_then(|s| s.get(json.get("scene").and_then(|v| v.as_u64()).unwrap_or(0) as usize))
        .and_then(|s| s.get("nodes"))
        .and_then(|n| n.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_u64()).map(|v| v as usize).collect())
        .unwrap_or_default();
    if !scene_roots.is_empty() {
        for r in scene_roots {
            g.walk(r, &IDENTITY, 0, &mut pts);
        }
    } else if let Some(nodes) = nodes {
        let children: std::collections::HashSet<u64> = nodes
            .iter()
            .filter_map(|n| n.get("children").and_then(|c| c.as_array()))
            .flatten()
            .filter_map(|c| c.as_u64())
            .collect();
        for i in 0..nodes.len() {
            if !children.contains(&(i as u64)) {
                g.walk(i, &IDENTITY, 0, &mut pts);
            }
        }
    } else {
        for i in 0..json.get("meshes").and_then(|m| m.as_array()).map_or(0, |m| m.len()) {
            let _ = g.add_mesh(i, &IDENTITY, &mut pts);
        }
    }
    (!pts.is_empty()).then_some(Mesh { pts, z_up: false, format: "glTF" })
}

// ── 3MF ─────────────────────────────────────────────────────────────

fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

fn xml_tags<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = xml[from..].find(&open) {
        let s = from + p;
        let after = xml.as_bytes().get(s + open.len()).copied().unwrap_or(b'>');
        let Some(e) = xml[s..].find('>') else { break };
        if after == b' ' || after == b'>' || after == b'/' || after == b'\n' || after == b'\t' {
            out.push(&xml[s..s + e + 1]);
        }
        from = s + e + 1;
    }
    out
}

/// 3MF affine transform (row-vector, 12 numbers) as a column-major Mat4.
fn mat_from_3mf(t: &str) -> Mat4 {
    let v: Vec<f32> = t.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if v.len() != 12 {
        return IDENTITY;
    }
    [
        v[0], v[1], v[2], 0.0, v[3], v[4], v[5], 0.0, v[6], v[7], v[8], 0.0, v[9], v[10], v[11], 1.0,
    ]
}

struct ThreeMfObject {
    verts: Vec<[f32; 3]>,
    tris: Vec<[usize; 3]>,
    components: Vec<(String, Mat4)>,
}

fn parse_3mf(path: &Path) -> Option<Mesh> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()?;
    let name = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .find(|n| n.to_ascii_lowercase().ends_with(".model"))?;
    let mut xml = String::new();
    zip.by_name(&name).ok()?.take(MAX_FILE_BYTES).read_to_string(&mut xml).ok()?;

    let mut objects: HashMap<String, ThreeMfObject> = HashMap::new();
    let mut rest = xml.as_str();
    while let Some(s) = rest.find("<object") {
        let after = &rest[s..];
        let Some(close) = after.find("</object>") else { break };
        let body = &after[..close];
        if let Some(id) = xml_tags(body, "object").first().and_then(|t| xml_attr(t, "id")) {
            let verts = xml_tags(body, "vertex")
                .iter()
                .map(|t| {
                    let n = |k| xml_attr(t, k).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                    [n("x"), n("y"), n("z")]
                })
                .collect();
            let tris = xml_tags(body, "triangle")
                .iter()
                .map(|t| {
                    let n = |k| xml_attr(t, k).and_then(|v| v.parse().ok()).unwrap_or(0usize);
                    [n("v1"), n("v2"), n("v3")]
                })
                .collect();
            let components = xml_tags(body, "component")
                .iter()
                .filter_map(|t| {
                    Some((
                        xml_attr(t, "objectid")?,
                        xml_attr(t, "transform").map(|x| mat_from_3mf(&x)).unwrap_or(IDENTITY),
                    ))
                })
                .collect();
            objects.insert(id, ThreeMfObject { verts, tris, components });
        }
        rest = &after[close + 9..];
    }

    fn emit(
        objs: &HashMap<String, ThreeMfObject>,
        id: &str,
        m: &Mat4,
        depth: usize,
        pts: &mut Vec<[f32; 3]>,
    ) {
        let Some(o) = objs.get(id) else { return };
        if depth > 16 {
            return;
        }
        for t in &o.tris {
            if let (Some(a), Some(b), Some(c)) = (o.verts.get(t[0]), o.verts.get(t[1]), o.verts.get(t[2])) {
                pts.extend([mat_apply(m, *a), mat_apply(m, *b), mat_apply(m, *c)]);
            }
        }
        for (cid, cm) in &o.components {
            emit(objs, cid, &mat_mul(m, cm), depth + 1, pts);
        }
    }

    let mut pts = Vec::new();
    let build = xml.find("<build").map(|s| &xml[s..]).unwrap_or("");
    let items = xml_tags(build, "item");
    if items.is_empty() {
        for id in objects.keys() {
            emit(&objects, id, &IDENTITY, 0, &mut pts);
        }
    } else {
        for item in items {
            if let Some(id) = xml_attr(item, "objectid") {
                let m = xml_attr(item, "transform").map(|x| mat_from_3mf(&x)).unwrap_or(IDENTITY);
                emit(&objects, &id, &m, 0, &mut pts);
            }
        }
    }
    (!pts.is_empty()).then_some(Mesh { pts, z_up: true, format: "3MF" })
}

// ── Blender: the thumbnail saved inside the file ────────────────────

/// Returns (width, height, RGBA, top row first).
fn blend_thumbnail(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let mut raw = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 4];
    raw.read_exact(&mut magic).ok()?;
    let raw = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    if magic[..2] == [0x1f, 0x8b] {
        // Only the start of the file is needed: the thumbnail block comes first.
        flate2::read::GzDecoder::new(raw).take(64 * 1024 * 1024).read_to_end(&mut data).ok()?;
    } else if magic == *b"BLEN" {
        raw.take(64 * 1024 * 1024).read_to_end(&mut data).ok()?;
    } else {
        // Newer Blender can compress with zstd; there is no decoder built in.
        return None;
    }
    if data.get(..7)? != b"BLENDER" {
        return None;
    }
    // Header: "BLENDER" + (`_`|`-`) + (`v`|`V`) + 3 digits, or the 17-byte
    // form "BLENDER17-01v0500" used by Blender 5.
    let (header_len, ptr8, big, large) = if data[7].is_ascii_digit() {
        (17usize, true, data[12] == b'V', true)
    } else {
        (12usize, data[7] == b'-', data[8] == b'V', false)
    };
    let rd32 = |at: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
        Some(if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    };
    let rd64 = |at: usize| -> Option<u64> {
        let b: [u8; 8] = data.get(at..at + 8)?.try_into().ok()?;
        Some(if big { u64::from_be_bytes(b) } else { u64::from_le_bytes(b) })
    };
    let mut pos = header_len;
    for _ in 0..64 {
        let code = data.get(pos..pos + 4)?;
        let (len, head) = if large {
            (rd64(pos + 16)? as usize, 32)
        } else {
            (rd32(pos + 4)? as usize, if ptr8 { 24 } else { 20 })
        };
        if code == b"TEST" {
            let body = data.get(pos + head..pos + head + len)?;
            let rd = |at: usize| -> Option<i32> {
                let b: [u8; 4] = body.get(at..at + 4)?.try_into().ok()?;
                Some(if big { i32::from_be_bytes(b) } else { i32::from_le_bytes(b) })
            };
            let (w, h) = (rd(0)?, rd(4)?);
            if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
                return None;
            }
            let (w, h) = (w as usize, h as usize);
            let px = body.get(8..8 + w * h * 4)?;
            // Blender stores rows bottom-up.
            let mut out = Vec::with_capacity(px.len());
            for row in (0..h).rev() {
                out.extend_from_slice(&px[row * w * 4..(row + 1) * w * 4]);
            }
            return Some((w as u32, h as u32, out));
        }
        if code == b"ENDB" || code == b"DNA1" {
            return None;
        }
        pos += head + len;
    }
    None
}

// ── Software renderer ───────────────────────────────────────────────

const BASE_COLOR: [f32; 3] = [0.66, 0.72, 0.82];

fn render(mesh: &Mesh, out_w: usize, out_h: usize) -> Vec<u8> {
    let (w, h) = (out_w * SUPERSAMPLE, out_h * SUPERSAMPLE);
    let Some((lo, hi)) = mesh.bounds() else {
        return vec![0; out_w * out_h * 4];
    };
    let center = [
        (lo[0] + hi[0]) / 2.0,
        (lo[1] + hi[1]) / 2.0,
        (lo[2] + hi[2]) / 2.0,
    ];
    // Bring the model to Y-up around the origin, then turn the camera to a
    // three-quarter view from above.
    let (yaw, pitch) = (0.62f32, 0.42f32);
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let xform = |p: &[f32; 3]| -> [f32; 3] {
        let (x, y, z) = (p[0] - center[0], p[1] - center[1], p[2] - center[2]);
        let (x, y, z) = if mesh.z_up { (x, z, -y) } else { (x, y, z) };
        let (x, z) = (x * cy + z * sy, -x * sy + z * cy);
        [x, y * cp - z * sp, y * sp + z * cp]
    };
    let radius = mesh
        .pts
        .iter()
        .map(|p| {
            let d = [p[0] - center[0], p[1] - center[1], p[2] - center[2]];
            d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
        })
        .fold(0.0f32, f32::max)
        .sqrt()
        .max(1e-9);
    let scale = 0.46 * (w.min(h) as f32) / radius;
    let (cx, cy_px) = (w as f32 / 2.0, h as f32 / 2.0);
    let project = |v: &[f32; 3]| -> [f32; 3] { [cx + v[0] * scale, cy_px - v[1] * scale, v[2]] };

    let key = {
        let l = [-0.45f32, 0.75, 0.55];
        let n = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
        [l[0] / n, l[1] / n, l[2] / n]
    };
    let mut color = vec![0u8; w * h * 4];
    let mut depth = vec![f32::NEG_INFINITY; w * h];
    let stride = mesh.tri_count().div_ceil(MAX_DRAWN_TRIS).max(1);

    for t in (0..mesh.tri_count()).step_by(stride) {
        let a = xform(&mesh.pts[t * 3]);
        let b = xform(&mesh.pts[t * 3 + 1]);
        let c = xform(&mesh.pts[t * 3 + 2]);
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let mut n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len < 1e-20 || !len.is_finite() {
            continue;
        }
        for k in &mut n {
            *k /= len;
        }
        // Two-sided: the camera looks down -Z, so a normal facing +Z is front.
        if n[2] < 0.0 {
            for k in &mut n {
                *k = -*k;
            }
        }
        let lit = 0.28 + 0.52 * (n[0] * key[0] + n[1] * key[1] + n[2] * key[2]).max(0.0) + 0.2 * n[2];
        let rgb = [
            (BASE_COLOR[0] * lit * 255.0).clamp(0.0, 255.0) as u8,
            (BASE_COLOR[1] * lit * 255.0).clamp(0.0, 255.0) as u8,
            (BASE_COLOR[2] * lit * 255.0).clamp(0.0, 255.0) as u8,
        ];

        let (pa, pb, pc) = (project(&a), project(&b), project(&c));
        let area = (pb[0] - pa[0]) * (pc[1] - pa[1]) - (pb[1] - pa[1]) * (pc[0] - pa[0]);
        if area.abs() < 1e-9 {
            continue;
        }
        let min_x = pa[0].min(pb[0]).min(pc[0]).floor().max(0.0) as usize;
        let max_x = (pa[0].max(pb[0]).max(pc[0]).ceil().min(w as f32 - 1.0)).max(0.0) as usize;
        let min_y = pa[1].min(pb[1]).min(pc[1]).floor().max(0.0) as usize;
        let max_y = (pa[1].max(pb[1]).max(pc[1]).ceil().min(h as f32 - 1.0)).max(0.0) as usize;
        if pa[0].max(pb[0]).max(pc[0]) < 0.0 || pa[1].max(pb[1]).max(pc[1]) < 0.0 {
            continue;
        }
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w0 = ((pb[0] - px) * (pc[1] - py) - (pb[1] - py) * (pc[0] - px)) / area;
                let w1 = ((pc[0] - px) * (pa[1] - py) - (pc[1] - py) * (pa[0] - px)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let z = w0 * pa[2] + w1 * pb[2] + w2 * pc[2];
                let i = y * w + x;
                if z > depth[i] {
                    depth[i] = z;
                    color[i * 4..i * 4 + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                }
            }
        }
    }

    // Box-filter down; weight colour by coverage so edges blend into
    // transparency instead of a dark halo.
    let mut out = vec![0u8; out_w * out_h * 4];
    let n = (SUPERSAMPLE * SUPERSAMPLE) as u32;
    for y in 0..out_h {
        for x in 0..out_w {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..SUPERSAMPLE {
                for dx in 0..SUPERSAMPLE {
                    let i = ((y * SUPERSAMPLE + dy) * w + x * SUPERSAMPLE + dx) * 4;
                    let al = color[i + 3] as u32;
                    r += color[i] as u32 * al;
                    g += color[i + 1] as u32 * al;
                    b += color[i + 2] as u32 * al;
                    a += al;
                }
            }
            if a > 0 {
                let o = (y * out_w + x) * 4;
                out[o] = (r / a) as u8;
                out[o + 1] = (g / a) as u8;
                out[o + 2] = (b / a) as u8;
                out[o + 3] = (a / n) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube_stl_ascii() -> String {
        let v = [
            [0, 0, 0], [1, 0, 0], [1, 1, 0], [0, 1, 0], [0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1],
        ];
        let f = [
            [0, 2, 1], [0, 3, 2], [4, 5, 6], [4, 6, 7], [0, 1, 5], [0, 5, 4],
            [1, 2, 6], [1, 6, 5], [2, 3, 7], [2, 7, 6], [3, 0, 4], [3, 4, 7],
        ];
        let mut s = String::from("solid cube\n");
        for t in f {
            s.push_str("facet normal 0 0 0\nouter loop\n");
            for i in t {
                s.push_str(&format!("vertex {} {} {}\n", v[i][0], v[i][1], v[i][2]));
            }
            s.push_str("endloop\nendfacet\n");
        }
        s
    }

    #[test]
    fn ascii_stl_renders_visible_pixels() {
        let mesh = parse_stl(cube_stl_ascii().as_bytes()).unwrap();
        assert_eq!(mesh.tri_count(), 12);
        let img = render(&mesh, 64, 48);
        assert_eq!(img.len(), 64 * 48 * 4);
        let covered = img.chunks(4).filter(|p| p[3] > 0).count();
        assert!(covered > 300, "cube covered only {covered} pixels");
        // The corner stays transparent.
        assert_eq!(img[3], 0);
    }

    #[test]
    fn binary_stl_round_trips() {
        let mut data = vec![0u8; 80];
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&[0u8; 12]);
        for v in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for c in v {
                data.extend_from_slice(&c.to_le_bytes());
            }
        }
        data.extend_from_slice(&[0, 0]);
        assert_eq!(parse_stl(&data).unwrap().tri_count(), 1);
    }

    #[test]
    fn obj_quad_with_negative_indices_makes_two_triangles() {
        let obj = b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf -4/1 -3/2 -2/3 -1/4\n";
        assert_eq!(parse_obj(obj).unwrap().tri_count(), 2);
    }

    #[test]
    fn ascii_and_binary_ply_load() {
        let ascii = b"ply\nformat ascii 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n0 0 0\n1 0 0\n0 1 0\n3 0 1 2\n";
        assert_eq!(parse_ply(ascii).unwrap().tri_count(), 1);
        let mut bin = b"ply\nformat binary_little_endian 1.0\nelement vertex 3\nproperty float x\nproperty float y\nproperty float z\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n".to_vec();
        for v in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for c in v {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        bin.push(3);
        for i in [0i32, 1, 2] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        assert_eq!(parse_ply(&bin).unwrap().tri_count(), 1);
    }

    #[test]
    fn gltf_with_embedded_buffer_loads() {
        let mut buf = Vec::new();
        for v in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for c in v {
                buf.extend_from_slice(&c.to_le_bytes());
            }
        }
        let b64 = {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for ch in buf.chunks(3) {
                let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
                s.push(T[(n >> 18) as usize & 63] as char);
                s.push(T[(n >> 12) as usize & 63] as char);
                s.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
                s.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
            }
            s
        };
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],
            "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}}}}]}}],
            "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3"}}],
            "bufferViews":[{{"buffer":0,"byteLength":36}}],
            "buffers":[{{"byteLength":36,"uri":"data:application/octet-stream;base64,{b64}"}}]}}"#
        );
        let mesh = parse_gltf(json.as_bytes(), Path::new("/tmp/x.gltf")).unwrap();
        assert_eq!(mesh.tri_count(), 1);
    }

    #[test]
    fn blend_thumbnail_is_flipped_to_top_down() {
        // Legacy 64-bit little-endian header, one TEST block with a 1x2 image.
        let mut f = b"BLENDER-v300".to_vec();
        let body: Vec<u8> = [1i32.to_le_bytes(), 2i32.to_le_bytes()]
            .concat()
            .into_iter()
            .chain([10, 0, 0, 255, 20, 0, 0, 255]) // bottom row first
            .collect();
        f.extend_from_slice(b"TEST");
        f.extend_from_slice(&(body.len() as u32).to_le_bytes());
        f.extend_from_slice(&[0u8; 8]);
        f.extend_from_slice(&[0u8; 8]);
        f.extend_from_slice(&body);
        f.extend_from_slice(b"ENDB");
        f.extend_from_slice(&[0u8; 20]);
        let dir = std::env::temp_dir().join(format!("spotty-blend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.blend");
        std::fs::write(&p, &f).unwrap();
        let (w, h, px) = blend_thumbnail(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((w, h), (1, 2));
        assert_eq!(px[0], 20);
        assert_eq!(px[4], 10);
    }
}
