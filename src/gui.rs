//! Native GUI renderer (SPEC §16): X11 via dlopen'd libxcb — zero link deps,
//! graceful fallback when no display. Software RGBX framebuffer →
//! xcb_put_image; core events decoded from the stable 32-byte wire format.
//! Rendering law: element mutation sets `dirty`, the next pump renders one
//! frame, then the loop sleeps in poll() until the next X event.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CString};
use std::os::unix::io::RawFd;
use std::rc::Rc;

use crate::font8x8::FONT8X8;
use crate::uiruntime::{UiElement, UiRuntime};
use crate::value::{FuncVal, Value};

/* ---------------- libxcb FFI ---------------- */

type XcbConn = *mut c_void;

#[repr(C)]
struct XcbScreenIter {
    data: *mut u8,
    rem: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: RawFd,
    events: i16,
    revents: i16,
}

struct Fns {
    connect: unsafe extern "C" fn(*const c_char, *mut c_int) -> XcbConn,
    has_error: unsafe extern "C" fn(XcbConn) -> c_int,
    disconnect: unsafe extern "C" fn(XcbConn),
    get_setup: unsafe extern "C" fn(XcbConn) -> *const u8,
    roots_iter: unsafe extern "C" fn(*const u8) -> XcbScreenIter,
    gen_id: unsafe extern "C" fn(XcbConn) -> u32,
    create_window: unsafe extern "C" fn(
        XcbConn, u8, u32, u32, i16, i16, u16, u16, u16, u16, u32, u32, *const u32,
    ) -> u32,
    create_gc: unsafe extern "C" fn(XcbConn, u32, u32, u32, *const u32) -> u32,
    free_gc: unsafe extern "C" fn(XcbConn, u32),
    map_window: unsafe extern "C" fn(XcbConn, u32) -> u32,
    flush: unsafe extern "C" fn(XcbConn) -> c_int,
    change_property: unsafe extern "C" fn(
        XcbConn, u8, u32, u32, u32, u8, u32, *const u8,
    ) -> u32,
    intern_atom: unsafe extern "C" fn(XcbConn, u8, u16, *const c_char) -> u32,
    intern_atom_reply: unsafe extern "C" fn(XcbConn, u32, *mut *mut c_void) -> *mut u8,
    put_image: unsafe extern "C" fn(
        XcbConn, u8, u32, u32, u16, u16, i16, i16, u8, u8, u32, *const u8,
    ) -> u32,
    poll_event: unsafe extern "C" fn(XcbConn) -> *mut u8,
    put_image_checked: unsafe extern "C" fn(
        XcbConn, u8, u32, u32, u16, u16, i16, i16, u8, u8, u32, *const u8,
    ) -> u32,
    request_check: unsafe extern "C" fn(XcbConn) -> c_int,
    get_fd: unsafe extern "C" fn(XcbConn) -> c_int,
}

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
}

unsafe fn sym<T>(lib: *mut c_void, name: &str) -> Result<T, String> {
    let c = CString::new(name).unwrap();
    let p = unsafe { dlsym(lib, c.as_ptr()) };
    if p.is_null() {
        return Err(format!("gui: libxcb missing {name}"));
    }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

struct Xcb {
    conn: XcbConn,
    fd: RawFd,
    root: u32,
    root_visual: u32,
    root_w: u32,
    root_h: u32,
    _lib: *mut c_void, // keep dlopen handle alive
}

/* ---------------- framebuffer ---------------- */

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

pub struct GuiApp {
    f: Fns,
    x: Xcb,
    win: u32,
    gc: u32,
    wm_delete: u32,
    fb_w: u32,
    fb_h: u32,
    fb: Vec<u32>, // 0xAARRGGBB, XRGB byte order on LE
    /// (element, rect) in paint order — rebuilt each render
    laid: Vec<(Rc<RefCell<UiElement>>, Rect)>,
    mouse: (i32, i32),
    mouse_down: bool,
    focused: Option<Rc<RefCell<UiElement>>>,
}

/* ---------------- prop helpers ---------------- */

fn prop_str(el: &UiElement, name: &str) -> Option<String> {
    el.get(name).map(|v| match v {
        Value::Str(s) => s.borrow().clone(),
        other => other.display(),
    })
}

fn prop_i64(el: &UiElement, name: &str) -> Option<i64> {
    match el.get(name)? {
        Value::Int(i) => Some(i as i64),
        Value::Float(f) => Some(f as i64),
        Value::Str(s) => s.borrow().trim().parse().ok(),
        _ => None,
    }
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

fn color_of(el: &UiElement, name: &str, default: u32) -> u32 {
    prop_str(el, name).map(|s| parse_color(&s)).unwrap_or(default)
}

/* ---------------- text ---------------- */

const SCALE: usize = 2;

fn glyph(ch: char) -> [u8; 8] {
    let i = ch as usize;
    if i < 128 {
        FONT8X8[i]
    } else {
        FONT8X8[b'?' as usize]
    }
}

fn char_w(scale: usize) -> i32 {
    (9 * scale) as i32
}

fn text_w(s: &str, scale: usize) -> i32 {
    s.chars().count() as i32 * char_w(scale)
}

fn text_h(scale: usize) -> i32 {
    (8 * scale) as i32
}

/* ---------------- layout: single pass, fills `laid` ---------------- */

fn layout(
    app: &mut GuiApp,
    el: &Rc<RefCell<UiElement>>,
    x: i32,
    y: i32,
    w: i32,
) -> i32 {
    let (kind, pad, explicit_h) = {
        let b = el.borrow();
        (
            b.kind.clone(),
            prop_i64(&b, "padding").unwrap_or(8) as i32,
            prop_i64(&b, "height"),
        )
    };
    let scale = if kind == "heading" { 3 } else { SCALE };
    let has_text = ["text", "value", "placeholder"]
        .iter()
        .any(|p| prop_str(&el.borrow(), p).is_some());

    // self height
    let mut self_h = match kind.as_str() {
        "window" => text_h(3) + pad * 2,
        "input" => text_h(SCALE) + 12,
        "button" => text_h(SCALE) + 16,
        _ => {
            if has_text {
                text_h(scale) + 8
            } else {
                0
            }
        }
    };
    if let Some(h) = explicit_h {
        self_h = h as i32;
    }
    self_h = self_h.max(0);

    app.laid.push((el.clone(), Rect { x, y, w, h: self_h }));

    let children = el.borrow().children.clone();
    if children.is_empty() {
        return self_h;
    }
    // children flow below this element's own content; a window's content
    // area starts under its title bar, not under its full explicit height
    let content_top = if kind == "window" {
        y + text_h(3) + pad
    } else {
        y + self_h + 4
    };
    let mut cy = content_top;
    let inner_w = (w - 2 * pad).max(20);
    for c in &children {
        let ch = layout(app, c, x + pad, cy, inner_w);
        cy += ch + 4;
    }
    (cy - y - 4 + pad).max(self_h)
}

/* ---------------- paint ---------------- */

impl GuiApp {
    fn px(&mut self, x: i32, y: i32, c: u32) {
        if x < 0 || y < 0 || x >= self.fb_w as i32 || y >= self.fb_h as i32 {
            return;
        }
        self.fb[y as usize * self.fb_w as usize + x as usize] = 0xff00_0000 | c;
    }

    fn rect(&mut self, r: Rect, c: u32) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                self.px(x, y, c);
            }
        }
    }

    fn border(&mut self, r: Rect, c: u32) {
        for x in r.x..r.x + r.w {
            self.px(x, r.y, c);
            self.px(x, r.y + r.h - 1, c);
        }
        for y in r.y..r.y + r.h {
            self.px(r.x, y, c);
            self.px(r.x + r.w - 1, y, c);
        }
    }

    fn draw_text(&mut self, x: i32, y: i32, s: &str, c: u32, scale: usize) {
        let mut cx = x;
        for ch in s.chars() {
            let g = glyph(ch);
            for gy in 0..8 {
                let bits = g[gy];
                if bits == 0 {
                    continue;
                }
                for gx in 0..8 {
                    if bits & (0x80 >> gx) != 0 {
                        let bx = cx + gx as i32 * scale as i32;
                        let by = y + gy as i32 * scale as i32;
                        for sy in 0..scale {
                            for sx in 0..scale {
                                self.px(bx + sx as i32, by + sy as i32, c);
                            }
                        }
                    }
                }
            }
            cx += char_w(scale);
        }
    }

    fn paint(&mut self, el: &Rc<RefCell<UiElement>>, r: Rect) {
        let kind = el.borrow().kind.clone();
        match kind.as_str() {
            "window" => {
                self.rect(r, color_of(&el.borrow(), "background", 0xf2f2f2));
                let pad = prop_i64(&el.borrow(), "padding").unwrap_or(8) as i32;
                let th = text_h(3) + pad;
                self.rect(Rect { x: r.x, y: r.y, w: r.w, h: th }, 0x2d2d2d);
                if let Some(t) = prop_str(&el.borrow(), "title") {
                    self.draw_text(r.x + 8, r.y + (th - text_h(2)) / 2, &t, 0xffffff, SCALE);
                }
            }
            "text" | "label" | "heading" => {
                let scale = if kind == "heading" { 3 } else { SCALE };
                if let Some(t) = prop_str(&el.borrow(), "text") {
                    let c = color_of(&el.borrow(), "color", 0x1a1a1a);
                    self.draw_text(r.x + 4, r.y + 4, &t, c, scale);
                }
            }
            "button" => {
                let in_rect = self.mouse.0 >= r.x && self.mouse.0 < r.x + r.w
                    && self.mouse.1 >= r.y && self.mouse.1 < r.y + r.h;
                let bg = if in_rect && self.mouse_down {
                    0x9a9a9a
                } else if in_rect {
                    0xd8d8d8
                } else {
                    color_of(&el.borrow(), "background", 0xe4e4e4)
                };
                self.rect(r, bg);
                self.border(r, if in_rect { 0x555555 } else { 0x999999 });
                if let Some(t) = prop_str(&el.borrow(), "text") {
                    let tw = text_w(&t, SCALE);
                    self.draw_text(
                        r.x + (r.w - tw) / 2,
                        r.y + (r.h - text_h(SCALE)) / 2,
                        &t,
                        0x1a1a1a,
                        SCALE,
                    );
                }
            }
            "input" => {
                let focused = self
                    .focused
                    .as_ref()
                    .map(|f| Rc::ptr_eq(f, el))
                    .unwrap_or(false);
                self.rect(r, color_of(&el.borrow(), "background", 0xffffff));
                self.border(r, if focused { 0x3366cc } else { 0xaaaaaa });
                let val = prop_str(&el.borrow(), "value")
                    .or_else(|| prop_str(&el.borrow(), "text"))
                    .unwrap_or_else(|| prop_str(&el.borrow(), "placeholder").unwrap_or_default());
                let has_val = prop_str(&el.borrow(), "value").is_some()
                    || prop_str(&el.borrow(), "text").is_some();
                self.draw_text(
                    r.x + 6,
                    r.y + (r.h - text_h(SCALE)) / 2,
                    &val,
                    if has_val { 0x1a1a1a } else { 0x999999 },
                    SCALE,
                );
            }
            _ => {
                let bg = color_of(&el.borrow(), "background", 0xffffff);
                if bg != 0 {
                    self.rect(r, bg);
                }
            }
        }
    }

    fn render(&mut self, ui: &UiRuntime) {
        self.laid.clear();
        self.fb.fill(0xfff2f2f2);
        if let Some(win) = ui.windows.first() {
            win.borrow_mut().dirty = false;
            let r = Rect { x: 0, y: 0, w: self.fb_w as i32, h: self.fb_h as i32 };
            let _ = layout(self, win, 0, 0, r.w);
            let laid = std::mem::take(&mut self.laid);
            for (el, rect) in &laid {
                self.paint(el, *rect);
            }
            self.laid = laid;
        }
        unsafe {
            let cookie = (self.f.put_image_checked)(
                self.x.conn, 2, self.win, self.gc, // Z_PIXMAP
                self.fb_w as u16, self.fb_h as u16,
                0, 0, 0, 24,
                self.fb.len() as u32 * 4,
                self.fb.as_ptr() as *const u8,
            );
            let _ = cookie;
            (self.f.flush)(self.x.conn);
            let err = (self.f.request_check)(self.x.conn);
            if err != 0 {
                eprintln!("gui: X error {err} on put_image");
            }
        }
    }

    /// Drain all pending X events; returns clicked element (if any).
    fn pump_events(&mut self) -> (bool, Option<Rc<RefCell<UiElement>>>) {
        let mut redraw = false;
        let mut clicked = None;
        loop {
            let p = unsafe { (self.f.poll_event)(self.x.conn) };
            if p.is_null() {
                break;
            }
            let buf = unsafe { std::slice::from_raw_parts(p, 32) };
            if buf.len() >= 32 {
                let t = buf[0] & 0x7f;
                let det = buf[1];
                let i16at = |o: usize| i16::from_ne_bytes([buf[o], buf[o + 1]]) as i32;
                let u32at = |o: usize| {
                    u32::from_ne_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
                };
                match t {
                    4 => {
                        // ButtonPress
                        if det == 1 {
                            self.mouse = (i16at(24), i16at(26));
                            self.mouse_down = true;
                            for (el, r) in self.laid.iter().rev() {
                                if self.mouse.0 >= r.x && self.mouse.0 < r.x + r.w
                                    && self.mouse.1 >= r.y && self.mouse.1 < r.y + r.h
                                {
                                    clicked = Some(el.clone());
                                    if el.borrow().kind == "input" {
                                        self.focused = Some(el.clone());
                                    }
                                    break;
                                }
                            }
                            redraw = true;
                        }
                    }
                    5 => {
                        if det == 1 {
                            self.mouse_down = false;
                            redraw = true;
                        }
                    }
                    6 => {
                        self.mouse = (i16at(24), i16at(26));
                        redraw = true;
                    }
                    2 => {
                        if det == 9 {
                            // Escape closes
                            unsafe { (self.f.disconnect)(self.x.conn) };
                            std::process::exit(0);
                        }
                    }
                    12 => redraw = true, // Expose
                    33 => {
                        // ClientMessage — WM_DELETE_WINDOW?
                        if u32at(8) == self.wm_delete {
                            unsafe { (self.f.disconnect)(self.x.conn) };
                            std::process::exit(0);
                        }
                    }
                    _ => {}
                }
            }
            unsafe { free(p as *mut c_void) };
        }
        (redraw, clicked)
    }

    /// Block on the X fd until an event arrives.
    fn wait(&mut self) {
        let mut pfd = PollFd { fd: self.x.fd, events: 1, revents: 0 };
        unsafe {
            poll(&mut pfd, 1, -1);
        }
    }
}

/* ---------------- public API ---------------- */

/// Open a native window for the first ui.window(). Ok(None) = no display.
pub fn open(ui: &UiRuntime) -> Result<Option<GuiApp>, String> {
    let Some(win) = ui.windows.first() else {
        return Ok(None);
    };
    let (w, h, title) = {
        let b = win.borrow();
        (
            prop_i64(&b, "width").unwrap_or(800).clamp(1, 8192) as u32,
            prop_i64(&b, "height").unwrap_or(600).clamp(1, 8192) as u32,
            prop_str(&b, "title").unwrap_or_else(|| "zap".into()),
        )
    };

    let lib = unsafe { dlopen(b"libxcb.so.1\0".as_ptr() as *const c_char, 2) };
    if lib.is_null() {
        return Err("gui: libxcb.so.1 not found".into());
    }
    macro_rules! sym {
        ($n:literal, $t:ty) => {
            unsafe { sym::<$t>(lib, $n) }?
        };
    }
    let f = Fns {
        connect: sym!("xcb_connect", unsafe extern "C" fn(*const c_char, *mut c_int) -> XcbConn),
        has_error: sym!("xcb_connection_has_error", unsafe extern "C" fn(XcbConn) -> c_int),
        disconnect: sym!("xcb_disconnect", unsafe extern "C" fn(XcbConn)),
        get_setup: sym!("xcb_get_setup", unsafe extern "C" fn(XcbConn) -> *const u8),
        roots_iter: sym!("xcb_setup_roots_iterator", unsafe extern "C" fn(*const u8) -> XcbScreenIter),
        gen_id: sym!("xcb_generate_id", unsafe extern "C" fn(XcbConn) -> u32),
        create_window: sym!("xcb_create_window", unsafe extern "C" fn(XcbConn,u8,u32,u32,i16,i16,u16,u16,u16,u16,u32,u32,*const u32)->u32),
        create_gc: sym!("xcb_create_gc", unsafe extern "C" fn(XcbConn,u32,u32,u32,*const u32)->u32),
        free_gc: sym!("xcb_free_gc", unsafe extern "C" fn(XcbConn,u32)),
        map_window: sym!("xcb_map_window", unsafe extern "C" fn(XcbConn,u32)->u32),
        flush: sym!("xcb_flush", unsafe extern "C" fn(XcbConn)->c_int),
        change_property: sym!("xcb_change_property", unsafe extern "C" fn(XcbConn,u8,u32,u32,u32,u8,u32,*const u8)->u32),
        intern_atom: sym!("xcb_intern_atom", unsafe extern "C" fn(XcbConn,u8,u16,*const c_char)->u32),
        intern_atom_reply: sym!("xcb_intern_atom_reply", unsafe extern "C" fn(XcbConn,u32,*mut *mut c_void)->*mut u8),
        put_image: sym!("xcb_put_image", unsafe extern "C" fn(XcbConn,u8,u32,u32,u16,u16,i16,i16,u8,u8,u32,*const u8)->u32),
        poll_event: sym!("xcb_poll_for_event", unsafe extern "C" fn(XcbConn)->*mut u8),
        put_image_checked: sym!("xcb_put_image_checked", unsafe extern "C" fn(XcbConn,u8,u32,u32,u16,u16,i16,i16,u8,u8,u32,*const u8)->u32),
        request_check: sym!("xcb_request_check", unsafe extern "C" fn(XcbConn)->c_int),
        get_fd: sym!("xcb_get_file_descriptor", unsafe extern "C" fn(XcbConn)->c_int),
    };

    let mut screen = 0;
    let conn = unsafe { (f.connect)(std::ptr::null(), &mut screen) };
    if conn.is_null() || unsafe { (f.has_error)(conn) } != 0 {
        return Ok(None); // no display → headless fallback
    }
    let setup = unsafe { (f.get_setup)(conn) };
    let it = unsafe { (f.roots_iter)(setup) };
    if it.data.is_null() {
        return Ok(None);
    }
    let s = it.data;
    let rd16 = |o: usize| unsafe { (s.add(o) as *const u16).read_unaligned() };
    let rd32 = |o: usize| unsafe { (s.add(o) as *const u32).read_unaligned() };
    let (root, root_visual, root_w, root_h) = (rd32(0), rd32(32), rd16(20) as u32, rd16(22) as u32);

    let intern = |name: &str| -> u32 {
        let c = CString::new(name).unwrap();
        let cookie = unsafe { (f.intern_atom)(conn, 0, c.as_bytes().len() as u16, c.as_ptr()) };
        let mut err: *mut c_void = std::ptr::null_mut();
        let reply = unsafe { (f.intern_atom_reply)(conn, cookie, &mut err) };
        if reply.is_null() {
            return 0;
        }
        let atom = unsafe { (reply.add(8) as *const u32).read_unaligned() };
        unsafe { free(reply as *mut c_void) };
        atom
    };

    let cw_event = 1u32 | 4 | 8 | 64 | 32768; // PRESS|RELEASE|MOTION|EXPOSURE
    let wid = unsafe { (f.gen_id)(conn) };
    unsafe {
        (f.create_window)(
            conn, 0, wid, root,
            100, 100, w as u16, h as u16, 0, 1, root_visual,
            2048, // CW_EVENT_MASK
            &cw_event,
        );
    }
    let wm_name = intern("WM_NAME");
    let string_atom = intern("STRING");
    let wm_proto = intern("WM_PROTOCOLS");
    let wm_delete = intern("WM_DELETE_WINDOW");
    unsafe {
        let t = CString::new(title).unwrap_or_else(|_| CString::new("zap").unwrap());
        (f.change_property)(conn, 0, wid, wm_name, string_atom, 8, t.as_bytes().len() as u32, t.as_ptr() as *const u8);
        (f.change_property)(conn, 0, wid, wm_proto, 4, 32, 1, wm_delete.to_ne_bytes().as_ptr());
        (f.map_window)(conn, wid);
        (f.flush)(conn);
    }
    let fd = unsafe { (f.get_fd)(conn) };
    let gc = unsafe { (f.gen_id)(conn) };
    unsafe { (f.create_gc)(conn, gc, wid, 0, std::ptr::null()) };

    Ok(Some(GuiApp {
        f,
        x: Xcb { conn, fd, root, root_visual, root_w, root_h, _lib: lib },
        win: wid,
        gc,
        wm_delete,
        fb_w: w,
        fb_h: h,
        fb: vec![0xfff2f2f2; (w * h) as usize],
        laid: Vec::new(),
        mouse: (-1, -1),
        mouse_down: false,
        focused: None,
    }))
}

/// Drive the GUI until close. `invoke` dispatches click handlers into the VM.
/// Returns number of frames rendered.
pub fn run(app: &mut GuiApp, ui: &UiRuntime, invoke: &mut dyn FnMut(Rc<FuncVal>) -> Result<(), String>) -> u64 {
    let mut frames = 0u64;
    // initial render (windows start dirty)
    if ui.windows.iter().any(|w| w.borrow().dirty) {
        app.render(ui);
        frames += 1;
    }
    loop {
        app.wait();
        let (redraw, clicked) = app.pump_events();
        if redraw || ui.windows.iter().any(|w| w.borrow().dirty) {
            app.render(ui);
            frames += 1;
        }
        if let Some(el) = clicked {
            let handlers: Vec<Rc<FuncVal>> = el
                .borrow()
                .handlers
                .iter()
                .filter(|(n, _)| n == "click")
                .map(|(_, f)| f.clone())
                .collect();
            for f in handlers {
                let _ = invoke(f);
            }
            if ui.windows.iter().any(|w| w.borrow().dirty) {
                app.render(ui);
                frames += 1;
            }
        }
    }
}
