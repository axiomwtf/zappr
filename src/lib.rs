//! zapgui — native GUI host, linked into the zap binary as an rlib.
//! The interpreter marshals its UiElement tree into the ZUiC layout
//! below and calls `zap_gui_run`; clicks are reported back through the
//! invoke callback. Linux backend: Wayland via dlopen'd
//! libwayland-client + libfontconfig/freetype (system libs, no shipped
//! .so). Windows backend: Win32 GDI.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_char, c_void, CStr};
use std::ptr;

/* ---------- C ABI for the interpreter↔renderer bridge ---------- */

pub type ZTag = i32;
pub const Z_NOTHING: ZTag = 0;
pub const Z_INT: ZTag = 1;
pub const Z_FLOAT: ZTag = 2;
pub const Z_STR: ZTag = 3;
pub const Z_BOOL: ZTag = 4;
pub const Z_BYTES: ZTag = 5;
pub const Z_LIST: ZTag = 6;
pub const Z_MAP: ZTag = 7;
pub const Z_TUPLE: ZTag = 8;
pub const Z_REC: ZTag = 9;
pub const Z_FUNC: ZTag = 10;
pub const Z_TYPE: ZTag = 11;
pub const Z_ERR: ZTag = 12;
pub const Z_POISON: ZTag = 13;
pub const Z_CHAN: ZTag = 14;
pub const Z_UI: ZTag = 15;
pub const Z_WEAK: ZTag = 16;
pub const Z_VARIANT: ZTag = 17;

#[repr(C)]
pub union ZValU {
    pub i: i64,
    pub f: f64,
    pub b: i32,
    pub s: *mut c_char,
    pub by: ZBytes,
    pub vec: *mut ZVecC,
    pub map: *mut c_void,
    pub rec: *mut c_void,
    pub fnp: *mut c_void,
    pub tname: *mut c_char,
    pub err: *mut c_void,
    pub poi: *mut c_void,
    pub chan: *mut c_void,
    pub ui: *mut ZUiC,
    pub weak: *mut c_void,
    pub var: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ZBytes {
    pub p: *mut u8,
    pub n: usize,
}

#[repr(C)]
pub struct ZValC {
    pub tag: ZTag,
    pub _pad: i32,
    pub aux: usize,
    pub u: ZValU,
}

#[repr(C)]
pub struct ZVecC {
    pub v: *mut ZValC,
    pub n: usize,
    pub cap: usize,
}

#[repr(C)]
pub struct ZPairC {
    pub k: ZValC,
    pub v: ZValC,
}

#[repr(C)]
pub struct ZUiC {
    pub kind: *mut c_char,
    pub props: *mut ZPairC,
    pub pn: usize,
    pub pcap: usize,
    pub ch: *mut *mut ZUiC,
    pub cn: usize,
    pub ccap: usize,
    pub handlers: ZValC,
}

pub type ZInvokeCb = Option<unsafe extern "C" fn(*const ZValC)>;
/// Re-marshal the live element tree; returns a fresh root. Called before
/// every render so prop mutations made by click handlers become visible.
pub type ZRefreshCb = Option<unsafe extern "C" fn() -> *mut ZUiC>;

/* ---------- font8x8 (Public Domain, Daniel Hepper) ---------- */

pub const FONT8X8: [[u8; 8]; 128] = include!("font8x8.inc");

/* ---------- shared helpers ---------- */

unsafe fn zstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(p) }.to_str().unwrap_or(""))
    }
}

fn prop_str(u: &ZUiC, name: &str) -> Option<String> {
    unsafe {
        for i in 0..u.pn {
            let k = &*u.props.add(i);
            if k.k.tag == Z_STR && zstr(k.k.u.s) == Some(name) {
                let v = &k.v;
                if v.tag == Z_STR {
                    return zstr(v.u.s).map(|s| s.to_string());
                }
                if v.tag == Z_INT {
                    return Some((v.u.i as i64).to_string());
                }
                if v.tag == Z_FLOAT {
                    return Some(format!("{}", v.u.f));
                }
                return None;
            }
        }
    }
    None
}

fn prop_i64(u: &ZUiC, name: &str, dflt: i64) -> i64 {
    unsafe {
        for i in 0..u.pn {
            let k = &*u.props.add(i);
            if k.k.tag == Z_STR && zstr(k.k.u.s) == Some(name) {
                let v = &k.v;
                return match v.tag {
                    Z_INT => v.u.i as i64,
                    Z_FLOAT => v.u.f as i64,
                    Z_STR => zstr(v.u.s).and_then(|s| s.trim().parse().ok()).unwrap_or(dflt),
                    _ => dflt,
                };
            }
        }
    }
    dflt
}

fn parse_color(s: &str) -> u32 {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return v;
        }
    }
    match s.trim().to_lowercase().as_str() {
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xcc3333,
        "green" => 0x2e8b57,
        "blue" => 0x3366cc,
        "gray" | "grey" => 0x888888,
        "darkgray" | "darkgrey" => 0x444444,
        "lightgray" | "lightgrey" => 0xcccccc,
        "yellow" => 0xcccc33,
        _ => 0x202020,
    }
}

fn color_of(u: &ZUiC, name: &str, dflt: u32) -> u32 {
    prop_str(u, name).map(|s| parse_color(&s)).unwrap_or(dflt)
}

fn kind_of(u: &ZUiC) -> String {
    unsafe { zstr(u.kind).unwrap_or("").to_string() }
}

/* ---------- font metrics ---------- */

const SCALE: usize = 2;

fn glyph(ch: char) -> [u8; 8] {
    let i = ch as usize;
    if i < 128 { FONT8X8[i] } else { FONT8X8[b'?' as usize] }
}
fn char_w(scale: usize) -> i32 { (9 * scale) as i32 }
fn text_w(s: &str, scale: usize) -> i32 { s.chars().count() as i32 * char_w(scale) }
fn text_h(scale: usize) -> i32 { (8 * scale) as i32 }

/* ---------- shared gui state ---------- */

#[derive(Clone, Copy)]
struct Rect { x: i32, y: i32, w: i32, h: i32 }

struct Laid {
    el: *mut ZUiC,
    r: Rect,
}

#[derive(Clone)]
pub struct TextOp {
    pub x: i32,
    pub y: i32, // baseline
    pub s: String,
    pub color: u32,
    pub fam: String,
    pub px: i32,
}

pub struct Gui {
    pub fb_w: u32,
    pub fb_h: u32,
    pub fb: Vec<u32>,
    pub laid: Vec<Laid>,
    pub texts: Vec<TextOp>,
    pub mouse: (i32, i32),
    pub mouse_down: bool,
    /// topmost element under the pointer last frame — repaint only when
    /// this changes, not on every motion event
    pub hover: Option<*mut ZUiC>,
}

/// topmost laid-out element containing (x, y), or None
pub fn hit_test(g: &Gui, x: i32, y: i32) -> Option<*mut ZUiC> {
    for l in g.laid.iter().rev() {
        let r = l.r;
        if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
            return Some(l.el);
        }
    }
    None
}

/// efficientRedraw (§16.2): diff two frames into damage rects
/// (x, y, w, h). Per-row changed spans (runs ≤8px apart merged) are
/// grouped into rects across identical rows; >64 rects falls back to
/// one full-frame rect.
pub fn diff_damage(cur: &[u32], prev: &[u32], w: usize, h: usize) -> Vec<(u32, u32, u32, u32)> {
    let n = w * h;
    if cur.len() != n || prev.len() != n {
        return Vec::from([(0, 0, w as u32, h as u32)]);
    }
    let mut rows: Vec<Vec<(u32, u32)>> = Vec::with_capacity(h);
    for y in 0..h {
        let row = &cur[y * w..(y + 1) * w];
        let old = &prev[y * w..(y + 1) * w];
        let mut spans: Vec<(u32, u32)> = Vec::new();
        let mut x = 0;
        while x < w {
            if row[x] != old[x] {
                let x0 = x;
                while x < w && row[x] != old[x] {
                    x += 1;
                }
                if let Some(l) = spans.last_mut() {
                    if (x0 as u32).saturating_sub(l.1) <= 8 {
                        l.1 = x as u32;
                        continue;
                    }
                }
                spans.push((x0 as u32, x as u32));
            } else {
                x += 1;
            }
        }
        rows.push(spans);
    }
    let mut out = Vec::new();
    let mut y0 = 0;
    while y0 < h {
        if rows[y0].is_empty() {
            y0 += 1;
            continue;
        }
        let spans = &rows[y0];
        let mut y1 = y0 + 1;
        while y1 < h && rows[y1] == *spans {
            y1 += 1;
        }
        for &(x0, x1) in spans {
            out.push((x0, y0 as u32, x1 - x0, (y1 - y0) as u32));
        }
        y0 = y1;
    }
    if out.len() > 64 {
        return Vec::from([(0, 0, w as u32, h as u32)]);
    }
    out
}

/// Text output strategy: bitmap font drawn into the framebuffer, or an
/// external renderer (Xft on linux, GDI on windows) that draws after
/// the framebuffer blit. `font`/`fontSize` element props select family
/// and pixel size; default is the platform's standard sans.
pub trait TextOut {
    /// true → paint() records TextOps instead of drawing into fb
    fn external(&self) -> bool { false }
    fn line_h(&self, px: i32) -> i32 { px + px / 3 }
    fn measure(&self, fam: &str, px: i32, s: &str) -> (i32, i32);
    /// draw all recorded TextOps (external engines only)
    fn blit(&self, _g: &mut Gui) {}
}

pub struct BitmapText;
impl TextOut for BitmapText {
    fn measure(&self, _fam: &str, px: i32, s: &str) -> (i32, i32) {
        let sc = ((px.max(8) + 7) / 8).max(1) as i32;
        (s.chars().count() as i32 * 9 * sc, 8 * sc)
    }
}

/// resolve font props: `font` = family name, `fontSize` = pixels
fn font_req(u: &ZUiC, kind: &str) -> (String, i32) {
    let px = prop_i64(u, "fontSize", if kind == "heading" { 22 } else { 13 }) as i32;
    let fam = prop_str(u, "font").unwrap_or_else(|| "DejaVu Sans".into());
    (fam, px.max(6))
}

/// baseline y for a text op inside rect r
fn baseline_y(r: Rect, th: i32, px: i32) -> i32 {
    r.y + (r.h - th) / 2 + px * 3 / 4
}

/* ---------- layout ---------- */

fn layout(g: &mut Gui, u: *mut ZUiC, x: i32, y: i32, w: i32, t: &dyn TextOut) -> i32 {
    let uref = unsafe { &*u };
    let kind = kind_of(uref);
    let pad = prop_i64(uref, "padding", 8) as i32;
    let eh = prop_i64(uref, "height", -1);
    let (_fam, px) = font_req(uref, &kind);
    let lh = t.line_h(px);
    let has_text = ["text", "value", "placeholder"].iter().any(|p| prop_str(uref, p).is_some());
    let is_win = kind == "window";

    let mut self_h = if is_win {
        pad
    } else if kind == "input" {
        lh + 12
    } else if kind == "button" {
        lh + 16
    } else if has_text {
        lh + 8
    } else {
        0
    };
    if eh >= 0 { self_h = eh as i32; }
    self_h = self_h.max(0);

    g.laid.push(Laid { el: u, r: Rect { x, y, w, h: self_h } });

    if uref.cn == 0 { return self_h; }
    let mut cy = if is_win { y + pad } else { y + self_h + 4 };
    let inner_w = (w - 2 * pad).max(20);
    for i in 0..uref.cn {
        let c = unsafe { *uref.ch.add(i) };
        let ch = layout(g, c, x + pad, cy, inner_w, t);
        cy += ch + 4;
    }
    (cy - y - 4 + pad).max(self_h)
}

/* ---------- paint ---------- */

fn paint_px(g: &mut Gui, x: i32, y: i32, c: u32) {
    if x < 0 || y < 0 || x >= g.fb_w as i32 || y >= g.fb_h as i32 { return; }
    g.fb[y as usize * g.fb_w as usize + x as usize] = 0xff00_0000 | c;
}
fn paint_rect(g: &mut Gui, r: Rect, c: u32) {
    for y in r.y..r.y + r.h { for x in r.x..r.x + r.w { paint_px(g, x, y, c); } }
}
fn paint_border(g: &mut Gui, r: Rect, c: u32) {
    for x in r.x..r.x + r.w { paint_px(g, x, r.y, c); paint_px(g, x, r.y + r.h - 1, c); }
    for y in r.y..r.y + r.h { paint_px(g, r.x, y, c); paint_px(g, r.x + r.w - 1, y, c); }
}
fn draw_text(g: &mut Gui, x: i32, y: i32, s: &str, c: u32, scale: usize) {
    let mut cx = x;
    for ch in s.chars() {
        let gl = glyph(ch);
        for gy in 0..8 {
            let bits = gl[gy];
            if bits == 0 { continue; }
            for gx in 0..8 {
                if bits & (0x80 >> gx) != 0 {
                    let bx = cx + gx as i32 * scale as i32;
                    let by = y + gy as i32 * scale as i32;
                    for sy in 0..scale { for sx in 0..scale {
                        paint_px(g, bx + sx as i32, by + sy as i32, c);
                    } }
                }
            }
        }
        cx += char_w(scale);
    }
}

fn paint(g: &mut Gui, u: &ZUiC, r: Rect, t: &dyn TextOut) {
    let kind = kind_of(u);
    let in_rect = g.mouse.0 >= r.x && g.mouse.0 < r.x + r.w && g.mouse.1 >= r.y && g.mouse.1 < r.y + r.h;
    match kind.as_str() {
        "window" => {
            paint_rect(g, r, color_of(u, "background", 0xf2f2f2));
        }
        "text" | "label" | "heading" => {
            if let Some(txt) = prop_str(u, "text") {
                let (fam, px) = font_req(u, &kind);
                let col = color_of(u, "color", 0x1a1a1a);
                emit_text(g, t, r.x + 4, r.y + 4, &txt, col, &fam, px, r);
            }
        }
        "button" => {
            let bg = if in_rect && g.mouse_down { 0x9a9a9a }
                else if in_rect { 0xd8d8d8 }
                else { color_of(u, "background", 0xe4e4e4) };
            paint_rect(g, r, bg);
            paint_border(g, r, if in_rect { 0x555555 } else { 0x999999 });
            if let Some(txt) = prop_str(u, "text") {
                let (fam, px) = font_req(u, &kind);
                let (tw, _) = t.measure(&fam, px, &txt);
                let col = 0x1a1a1a;
                emit_text(g, t, r.x + (r.w - tw) / 2, r.y + 4, &txt, col, &fam, px, r);
            }
        }
        "input" => {
            paint_rect(g, r, color_of(u, "background", 0xffffff));
            paint_border(g, r, 0xaaaaaa);
            let val = prop_str(u, "value").or_else(|| prop_str(u, "text"))
                .or_else(|| prop_str(u, "placeholder")).unwrap_or_default();
            let has = prop_str(u, "value").is_some() || prop_str(u, "text").is_some();
            let (fam, px) = font_req(u, &kind);
            let col = if has { 0x1a1a1a } else { 0x999999 };
            emit_text(g, t, r.x + 6, r.y + 4, &val, col, &fam, px, r);
        }
        _ => {
            let bg = color_of(u, "background", 0xffffff);
            if bg != 0 { paint_rect(g, r, bg); }
        }
    }
}

/// draw text at (x, top_y) inside rect r — external engines get a
/// baseline-adjusted TextOp, bitmap draws immediately.
fn emit_text(g: &mut Gui, t: &dyn TextOut, x: i32, top: i32, s: &str,
             color: u32, fam: &str, px: i32, r: Rect) {
    if t.external() {
        let (_, th) = t.measure(fam, px, s);
        let _ = th;
        g.texts.push(TextOp {
            x, y: baseline_y(r, th, px), s: s.to_string(),
            color, fam: fam.to_string(), px,
        });
    } else {
        let sc = ((px.max(8) + 7) / 8).max(1);
        draw_text(g, x, top, s, color, sc as usize);
    }
}

// ZUiC dirty flag: C runtime has none; treat render-on-event as the dirty path.
trait DirtyClear { fn dirty_flag_clear(&mut self); }
impl DirtyClear for ZUiC {
    fn dirty_flag_clear(&mut self) {}
}

#[cfg(unix)]
mod wayland;
#[cfg(target_os = "windows")]
mod win32;
#[cfg(target_os = "wasi")]
mod wasi_gui;
#[cfg(target_os = "wasi")]
pub use wasi_gui::*;

// bridge fns used by the platform backend — operate on the shared Gui type
fn layout_paint(g: &mut Gui, root: *mut ZUiC, t: &dyn TextOut) {
    g.texts.clear();
    g.laid.clear();
    let w = g.fb_w as i32;
    layout(g, root, 0, 0, w, t);
    let laid = core::mem::take(&mut g.laid);
    for l in &laid {
        let el = unsafe { &*l.el };
        paint(g, el, l.r, t);
    }
    g.laid = laid;
}

/* ---------- click dispatch ---------- */

unsafe fn fire_click(el: *mut ZUiC, invoke: ZInvokeCb) {
    let hv = &(*el).handlers;
    if hv.tag == Z_LIST && !hv.u.vec.is_null() {
        let vec = &*hv.u.vec;
        if !vec.v.is_null() {
            for i in 0..vec.n {
                let fv = &*vec.v.add(i);
                if fv.tag == Z_FUNC {
                    if let Some(cb) = invoke {
                        cb(&*vec.v.add(i));
                    }
                }
            }
        }
    }
}

/* ---------- public entry ---------- */

unsafe fn refresh_root(root: &mut *mut ZUiC, refresh: ZRefreshCb) {
    if let Some(cb) = refresh {
        let n = unsafe { cb() };
        if !n.is_null() {
            *root = n;
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zap_gui_run(root: *mut ZUiC, invoke: ZInvokeCb, refresh: ZRefreshCb, efficient: i32) -> i32 {
    #[cfg(unix)]
    {
        let Some(win) = (unsafe { root.as_ref() }) else { return 0 };
        let mut root = root;
        match wayland::open(win, efficient != 0) {
            Ok(Some((mut xc, mut g))) => {
                wayland::render(&mut xc, &mut g, root);
                loop {
                    match wayland::events(&mut xc, &mut g) {
                        wayland::PumpResult::Close => break,
                        wayland::PumpResult::Events(clicked) => {
                            unsafe { refresh_root(&mut root, refresh) };
                            wayland::render(&mut xc, &mut g, root);
                            if let Some(el) = clicked {
                                unsafe { fire_click(el, invoke) };
                                unsafe { refresh_root(&mut root, refresh) };
                                wayland::render(&mut xc, &mut g, root);
                            }
                        }
                    }
                }
                return 1;
            }
            _ => return 0,
        }
    }
    #[cfg(target_os = "windows")]
    {
        let Some(win) = (unsafe { root.as_ref() }) else { return 0 };
        let mut root = root;
        match win32::open(win, efficient != 0) {
            Ok(Some((mut xc, mut g))) => {
                win32::render(&mut xc, &mut g, root);
                loop {
                    match win32::events(&mut xc, &mut g) {
                        win32::PumpResult::Close => break,
                        win32::PumpResult::Events(clicked) => {
                            unsafe { refresh_root(&mut root, refresh) };
                            win32::render(&mut xc, &mut g, root);
                            if let Some(el) = clicked {
                                unsafe { fire_click(el, invoke) };
                                unsafe { refresh_root(&mut root, refresh) };
                                win32::render(&mut xc, &mut g, root);
                            }
                        }
                    }
                }
                return 1;
            }
            _ => return 0,
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        // no native windowing on this target (wasi/web): headless
        let _ = (root, invoke, refresh, efficient);
        0
    }
}
