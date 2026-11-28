//! wasi/web GUI backend: the wasm host cannot block on a native event
//! loop, so `zap_gui_attach` registers the marshalled tree and returns;
//! the JS shim then drives the loop: `zap_gui_feed` queues pointer
//! events, `zap_gui_step` (called once per animation frame) applies
//! them, re-renders into an exported framebuffer, and reports damage
//! rects for `efficientRedraw`.

use super::{diff_damage, fire_click, hit_test, layout_paint, BitmapText, Gui, ZInvokeCb,
            ZRefreshCb, ZUiC};
use std::cell::UnsafeCell;

const KIND_MOVE: i32 = 0;
const KIND_DOWN: i32 = 1;
const KIND_UP: i32 = 2;

struct WasiGui {
    root: *mut ZUiC,
    invoke: ZInvokeCb,
    refresh: ZRefreshCb,
    efficient: bool,
    g: Gui,
    prev: Vec<u32>,
    events: Vec<(i32, i32, i32)>,
    had_press: bool,
    /// a frame is owed to JS (set by attach and after each event batch)
    pending: bool,
}

struct Shared(UnsafeCell<Option<Box<WasiGui>>>, UnsafeCell<Vec<u32>>);
unsafe impl Sync for Shared {}
static SHARED: Shared = Shared(UnsafeCell::new(None), UnsafeCell::new(Vec::new()));

unsafe fn wg() -> &'static mut WasiGui {
    unsafe { &mut *(*SHARED.0.get()).as_mut().unwrap() }
}

/// Render the current tree into g.fb and compute damage vs prev.
/// Returns the number of damage rects written to the damage buffer.
unsafe fn render_frame(st: &mut WasiGui) -> usize {
    let mut root = st.root;
    super::refresh_root(&mut root, st.refresh);
    st.root = root;
    let w = st.g.fb_w as usize;
    let h = st.g.fb_h as usize;
    if st.g.fb.len() != w * h {
        st.g.fb = vec![0u32; w * h];
        st.prev = vec![0u32; w * h];
    }
    layout_paint(&mut st.g, st.root, &BitmapText);
    let rects: Vec<(u32, u32, u32, u32)> = if st.efficient {
        diff_damage(&st.g.fb, &st.prev, w, h)
    } else {
        vec![(0, 0, st.g.fb_w, st.g.fb_h)]
    };
    st.prev.copy_from_slice(&st.g.fb);
    unsafe {
        let d = &mut *SHARED.1.get();
        d.clear();
        for r in &rects {
            d.extend_from_slice(&[r.0, r.1, r.2, r.3]);
        }
    }
    rects.len()
}

/// JS calls this once after `_start` resolves; nonzero → GUI mode.
#[unsafe(no_mangle)]
pub extern "C" fn zap_gui_active() -> i32 {
    unsafe { if (*SHARED.0.get()).is_some() { 1 } else { 0 } }
}

/// Called by the interpreter bridge instead of the blocking
/// `zap_gui_run` on wasi. Paints the first frame and returns; the
/// JS shim owns the event loop from here on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zap_gui_attach(
    root: *mut ZUiC,
    invoke: ZInvokeCb,
    refresh: ZRefreshCb,
    w: i32,
    h: i32,
    efficient: i32,
) -> i32 {
    unsafe {
        if root.is_null() || (*SHARED.0.get()).is_some() {
            return 0;
        }
        let (w, h) = (w.max(1) as u32, h.max(1) as u32);
        let mut st = Box::new(WasiGui {
            root,
            invoke,
            refresh,
            efficient: efficient != 0,
            g: Gui {
                fb_w: w,
                fb_h: h,
                fb: vec![0u32; (w * h) as usize],
                laid: Vec::new(),
                texts: Vec::new(),
                mouse: (-1, -1),
                mouse_down: false,
                hover: None,
            },
            prev: vec![0u32; (w * h) as usize],
            events: Vec::new(),
            had_press: false,
            pending: true,
        });
        // no pre-render: the first zap_gui_step produces the frame
        // (diffing against the zeroed prev buffer yields full damage)
        *SHARED.0.get() = Some(st);
        1
    }
}

/// Queue a pointer event from JS. kind: 0=move, 1=down, 2=up;
/// coords are framebuffer pixels.
#[unsafe(no_mangle)]
pub extern "C" fn zap_gui_feed(x: i32, y: i32, kind: i32) {
    unsafe {
        if (*SHARED.0.get()).is_none() {
            return;
        }
        wg().events.push((x, y, kind));
    }
}

/// One animation-frame step: drain queued events, hit-test clicks,
/// re-render if anything changed. Returns the damage-rect count for
/// the frame just produced (0 → nothing new to upload).
#[unsafe(no_mangle)]
pub extern "C" fn zap_gui_step() -> i32 {
    unsafe {
        if (*SHARED.0.get()).is_none() {
            return 0;
        }
        let st = wg();
        if st.events.is_empty() && !st.pending {
            return 0;
        }
        st.pending = false;
        let events = std::mem::take(&mut st.events);
        for (x, y, kind) in events {
            st.g.mouse = (x, y);
            match kind {
                KIND_MOVE => {}
                KIND_DOWN => {
                    st.g.mouse_down = true;
                    st.had_press = true;
                }
                KIND_UP => {
                    st.g.mouse_down = false;
                    if st.had_press {
                        st.had_press = false;
                        if let Some(el) = hit_test(&st.g, x, y) {
                            fire_click(el, st.invoke);
                        }
                    }
                }
                _ => {}
            }
        }
        let n = render_frame(st);
        n as i32
    }
}

/// Byte pointer to the RGBA-order framebuffer (w*h*4 bytes at
/// `zap_gui_size` dimensions). Pixel format is 0xAARRGGBB stored
/// little-endian → bytes are B,G,R,A; the JS shim swizzles to RGBA.
#[unsafe(no_mangle)]
pub extern "C" fn zap_gui_fb() -> usize {
    unsafe {
        if (*SHARED.0.get()).is_none() {
            return 0;
        }
        wg().g.fb.as_ptr() as usize
    }
}

/// Packed framebuffer size: low 32 bits = width, high 32 = height.
#[unsafe(no_mangle)]
pub extern "C" fn zap_gui_size() -> u64 {
    unsafe {
        if (*SHARED.0.get()).is_none() {
            return 0;
        }
        let g = &wg().g;
        (g.fb_w as u64) | ((g.fb_h as u64) << 32)
    }
}

/// Pointer to the damage-rect array (x,y,w,h u32 quads).
#[unsafe(no_mangle)]
pub extern "C" fn zap_gui_damage_ptr() -> usize {
    unsafe { (*SHARED.1.get()).as_ptr() as usize }
}
