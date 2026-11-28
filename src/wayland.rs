#![allow(unsafe_op_in_unsafe_fn)]
//! Wayland backend for the native GUI host (SPEC §16).
//! libwayland-client is dlopen'd at runtime — zero link deps, graceful
//! fallback to the headless element dump when no compositor is present.
//! Text is rendered into the shm buffer by a fontconfig+freetype engine,
//! also dlopen'd (libfreetype.so.6, libfontconfig.so.1).

use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::ptr;

/* ================= wayland-client FFI ================= */

type WlProxy = *mut c_void;

unsafe impl Sync for WlMessage {}
unsafe impl Sync for WlInterface {}

#[repr(C)]
struct WlMessage {
    name: *const c_char,
    signature: *const c_char,
    types: *const *const WlInterface,
}

#[repr(C)]
pub struct WlInterface {
    name: *const c_char,
    version: c_int,
    method_count: c_int,
    methods: *const WlMessage,
    event_count: c_int,
    events: *const WlMessage,
}

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
    fn ftruncate(fd: c_int, len: i64) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}
const POLLIN: i16 = 0x001;
const POLLNVAL: i16 = 0x020;

unsafe fn wsym<T>(lib: *mut c_void, name: &str) -> Result<T, ()> {
    let c = CString::new(name).unwrap();
    let p = unsafe { dlsym(lib, c.as_ptr()) };
    if p.is_null() { return Err(()); }
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

macro_rules! cs { ($s:literal) => { concat!($s, "\0").as_ptr() as *const c_char } }

pub struct Wl {
    lib: *mut c_void,
    connect: unsafe extern "C" fn(*const c_char) -> WlProxy,
    disconnect: unsafe extern "C" fn(WlProxy),
    roundtrip: unsafe extern "C" fn(WlProxy) -> c_int,
    dispatch: unsafe extern "C" fn(WlProxy) -> c_int,
    dispatch_pending: unsafe extern "C" fn(WlProxy) -> c_int,
    prepare_read: unsafe extern "C" fn(WlProxy) -> c_int,
    read_events: unsafe extern "C" fn(WlProxy) -> c_int,
    cancel_read: unsafe extern "C" fn(WlProxy),
    get_fd: unsafe extern "C" fn(WlProxy) -> c_int,
    flush: unsafe extern "C" fn(WlProxy) -> c_int,
    add_listener: unsafe extern "C" fn(WlProxy, *const c_void, *mut c_void) -> c_int,
    marshal: unsafe extern "C" fn(WlProxy, u32, ...),
    marshal_ctor: unsafe extern "C" fn(WlProxy, u32, *const WlInterface, u32, ...) -> WlProxy,
    proxy_destroy: unsafe extern "C" fn(WlProxy),
    // exported core interfaces
    i_registry: *const WlInterface,
    i_compositor: *const WlInterface,
    i_surface: *const WlInterface,
    i_shm: *const WlInterface,
    i_shm_pool: *const WlInterface,
    i_buffer: *const WlInterface,
    i_pointer: *const WlInterface,
    i_seat: *const WlInterface,
    i_callback: *const WlInterface,
}

unsafe fn wl_load() -> Result<Wl, String> {
    let lib = unsafe { dlopen(cs!("libwayland-client.so.0"), 2) };
    if lib.is_null() { return Err("libwayland-client not found".into()); }
    macro_rules! s { ($n:literal, $t:ty) => { match wsym::<$t>(lib, $n) { Ok(v) => v, Err(()) => return Err(concat!("missing wayland symbol ", $n).into()) } } }
    unsafe fn iface(lib: *mut c_void, name: &str) -> Result<*const WlInterface, String> {
        match wsym::<*const WlInterface>(lib, name) { Ok(v) => Ok(v), Err(()) => Err(format!("missing {}", name)) }
    }
    Ok(unsafe {
        Wl {
            lib,
            connect: s!("wl_display_connect", unsafe extern "C" fn(*const c_char) -> WlProxy),
            disconnect: s!("wl_display_disconnect", unsafe extern "C" fn(WlProxy)),
            roundtrip: s!("wl_display_roundtrip", unsafe extern "C" fn(WlProxy) -> c_int),
            dispatch: s!("wl_display_dispatch", unsafe extern "C" fn(WlProxy) -> c_int),
            dispatch_pending: s!("wl_display_dispatch_pending", unsafe extern "C" fn(WlProxy) -> c_int),
            prepare_read: s!("wl_display_prepare_read", unsafe extern "C" fn(WlProxy) -> c_int),
            read_events: s!("wl_display_read_events", unsafe extern "C" fn(WlProxy) -> c_int),
            cancel_read: s!("wl_display_cancel_read", unsafe extern "C" fn(WlProxy)),
            get_fd: s!("wl_display_get_fd", unsafe extern "C" fn(WlProxy) -> c_int),
            flush: s!("wl_display_flush", unsafe extern "C" fn(WlProxy) -> c_int),
            add_listener: s!("wl_proxy_add_listener", unsafe extern "C" fn(WlProxy, *const c_void, *mut c_void) -> c_int),
            marshal: s!("wl_proxy_marshal", unsafe extern "C" fn(WlProxy, u32, ...)),
            marshal_ctor: s!("wl_proxy_marshal_constructor_versioned", unsafe extern "C" fn(WlProxy, u32, *const WlInterface, u32, ...) -> WlProxy),
            proxy_destroy: s!("wl_proxy_destroy", unsafe extern "C" fn(WlProxy)),
            i_registry: iface(lib, "wl_registry_interface")?,
            i_compositor: iface(lib, "wl_compositor_interface")?,
            i_surface: iface(lib, "wl_surface_interface")?,
            i_shm: iface(lib, "wl_shm_interface")?,
            i_shm_pool: iface(lib, "wl_shm_pool_interface")?,
            i_buffer: iface(lib, "wl_buffer_interface")?,
            i_pointer: iface(lib, "wl_pointer_interface")?,
            i_seat: iface(lib, "wl_seat_interface")?,
            i_callback: iface(lib, "wl_callback_interface")?,
        }
    })
}

/* ---- xdg-shell interfaces (built at runtime: types[] need the ----
   ---- dlopen'd wl_surface_interface pointer) ---- */

pub struct Xdg {
    pub wm_base: *const WlInterface,
    pub surface: *const WlInterface,
    pub toplevel: *const WlInterface,
}

unsafe fn msg(name: &str, sig: &str, types: &[*const WlInterface]) -> WlMessage {
    // leak a NUL-terminated copy of name/sig and the types slice
    let nb = CString::new(name).unwrap();
    let sb = CString::new(sig).unwrap();
    let tp = if types.is_empty() { ptr::null() } else {
        let b = Box::new(types.to_vec());
        Box::leak(b).as_ptr() as *const *const WlInterface
    };
    // CString buffers must outlive the message — leak them too
    let np = nb.into_bytes_with_nul().leak().as_ptr() as *const c_char;
    let sp = sb.into_bytes_with_nul().leak().as_ptr() as *const c_char;
    WlMessage { name: np, signature: sp, types: tp }
}

unsafe fn iface(name: &str, version: i32, methods: Vec<WlMessage>, events: Vec<WlMessage>) -> *const WlInterface {
    let mb = Box::leak(methods.into_boxed_slice());
    let (ep, ec) = if events.is_empty() { (ptr::null(), 0) } else {
        let eb = Box::leak(events.into_boxed_slice());
        (eb.as_ptr(), eb.len() as i32)
    };
    let nb = CString::new(name).unwrap();
    let np = nb.into_bytes_with_nul().leak().as_ptr() as *const c_char;
    Box::leak(Box::new(WlInterface {
        name: np, version, method_count: mb.len() as i32,
        methods: mb.as_ptr(), event_count: ec, events: ep,
    }))
}

unsafe fn xdg_ifaces(wl_surface_iface: *const WlInterface) -> Xdg {
    // toplevel first (no iface deps in its messages)
    let tl_methods = vec![
        msg("destroy", "", &[]),
        msg("set_parent", "?o", &[ptr::null()]),
        msg("set_title", "s", &[]),
        msg("set_app_id", "s", &[]),
        msg("show_window_menu", "ouii", &[ptr::null(), ptr::null(), ptr::null()]),
        msg("move", "ou", &[ptr::null(), ptr::null()]),
        msg("resize", "ouu", &[ptr::null(), ptr::null(), ptr::null()]),
        msg("set_max_size", "ii", &[]),
        msg("set_min_size", "ii", &[]),
        msg("set_maximized", "", &[]),
        msg("unset_maximized", "", &[]),
        msg("set_fullscreen", "?o", &[ptr::null()]),
        msg("unset_fullscreen", "", &[]),
        msg("set_minimized", "", &[]),
    ];
    let tl_events = vec![
        msg("configure", "iia", &[]),
        msg("close", "", &[]),
        msg("configure_bounds", "4ii", &[]),
        msg("wm_capabilities", "5a", &[]),
    ];
    let toplevel = iface("xdg_toplevel", 1, tl_methods, tl_events);

    // xdg_surface: get_toplevel types -> [xdg_toplevel]
    let xs_methods = vec![
        msg("destroy", "", &[]),
        msg("get_toplevel", "n", &[toplevel]),
        msg("get_popup", "n?oo", &[ptr::null(), ptr::null(), ptr::null()]),
        msg("set_window_geometry", "iiii", &[]),
        msg("ack_configure", "u", &[]),
    ];
    let xs_events = vec![msg("configure", "u", &[])];
    let surface = iface("xdg_surface", 1, xs_methods, xs_events);

    // xdg_wm_base: get_xdg_surface types -> [xdg_surface, wl_surface]
    let wm_methods = vec![
        msg("destroy", "", &[]),
        msg("create_positioner", "n", &[ptr::null()]),
        msg("get_xdg_surface", "no", &[surface, wl_surface_iface]),
        msg("pong", "u", &[]),
    ];
    let wm_events = vec![msg("ping", "u", &[])];
    let wm_base = iface("xdg_wm_base", 1, wm_methods, wm_events);

    Xdg { wm_base, surface, toplevel }
}

/* ---- listener fn tables (first field = event 0) ---- */

unsafe extern "C" fn noop0() {}
unsafe extern "C" fn noop1(_: *mut c_void) {}
unsafe extern "C" fn noop2(_: *mut c_void, _: *mut c_void) {}
unsafe extern "C" fn noop3(_: *mut c_void, _: *mut c_void, _: u32) {}
unsafe extern "C" fn noop4(_: *mut c_void, _: *mut c_void, _: *mut c_void, _: u32) {}
unsafe extern "C" fn noop5(_: *mut c_void, _: *mut c_void, _: u32, _: u32) {}
unsafe extern "C" fn noop6(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32) {}
unsafe extern "C" fn noop7(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32) {}
unsafe extern "C" fn noop8(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32, _: u32) {}
unsafe extern "C" fn noop9(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32) {}
unsafe extern "C" fn noop10(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32) {}
unsafe extern "C" fn noop11(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32) {}
unsafe extern "C" fn noop12(_: *mut c_void, _: *mut c_void, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32) {}

#[repr(C)]
struct RegListener {
    global: unsafe extern "C" fn(*mut c_void, WlProxy, u32, *const c_char, u32),
    global_remove: unsafe extern "C" fn(*mut c_void, WlProxy, u32),
}

#[repr(C)]
struct SeatListener {
    capabilities: unsafe extern "C" fn(*mut c_void, WlProxy, u32),
    name: unsafe extern "C" fn(*mut c_void, WlProxy, *const c_char),
}

#[repr(C)]
struct PointerListener {
    enter: unsafe extern "C" fn(*mut c_void, WlProxy, u32, WlProxy, i32, i32),
    leave: unsafe extern "C" fn(*mut c_void, WlProxy, u32, WlProxy),
    motion: unsafe extern "C" fn(*mut c_void, WlProxy, u32, i32, i32),
    button: unsafe extern "C" fn(*mut c_void, WlProxy, u32, u32, u32, u32),
    axis: unsafe extern "C" fn(*mut c_void, WlProxy, u32, u32, i32),
    _pad: [usize; 8],
}

#[repr(C)]
struct WmBaseListener {
    ping: unsafe extern "C" fn(*mut c_void, WlProxy, u32),
    _pad: [usize; 4],
}

#[repr(C)]
struct XdgSurfaceListener {
    configure: unsafe extern "C" fn(*mut c_void, WlProxy, u32),
    _pad: [usize; 4],
}

#[repr(C)]
struct XdgToplevelListener {
    configure: unsafe extern "C" fn(*mut c_void, WlProxy, i32, i32, *mut c_void),
    close: unsafe extern "C" fn(*mut c_void, WlProxy),
    configure_bounds: unsafe extern "C" fn(*mut c_void, WlProxy, i32, i32),
    wm_capabilities: unsafe extern "C" fn(*mut c_void, WlProxy, *mut c_void),
}

/* ---- connection state ---- */

#[derive(Clone,Copy)]
pub struct Global { name: u32, version: u32 }

pub struct WConn {
    pub f: Wl,
    pub dpy: WlProxy,
    pub registry: WlProxy,
    pub compositor: WlProxy,
    pub shm: WlProxy,
    pub seat: WlProxy,
    pub pointer: WlProxy,
    pub wm_base: WlProxy,
    pub xdg_surface: WlProxy,
    pub xdg_toplevel: WlProxy,
    pub surface: WlProxy,
    pub pool: WlProxy,
    pub buffers: [WlProxy; 2],
    pub shm_data: *mut u8,
    pub shm_len: usize,
    pub buf_w: u32,
    pub buf_h: u32,
    pub cur: usize,
    pub g_comp: Option<Global>,
    pub g_shm: Option<Global>,
    pub g_seat: Option<Global>,
    pub g_wm: Option<Global>,
    pub closed: bool,
    pub configure_w: i32,
    pub configure_h: i32,
    pub have_configure: bool,
    pub configured: bool,
    pub pending_serial: u32,
    pub have_serial: bool,
    pub click: Option<(i32, i32, bool)>, // x, y, pressed
    pub had_press: bool,
    pub dirty: bool,
    /// per-slot: compositor released the buffer back to us
    buf_free: [bool; 2],
    /// efficientRedraw (§16.2): diff frames, upload/damage only changes
    pub efficient: bool,
    /// shadow copy of each shm buffer's pixel content
    slot_img: [Vec<u32>; 2],
    /// slot holding the last committed frame (damage diff target)
    last_slot: usize,
    pool_fd: c_int,
    pub mouse: (i32, i32),
    pub mouse_down: bool,
    pub text: Option<ft::Ft>,
}

/* ---- registry / listeners ---- */

unsafe extern "C" fn reg_global(data: *mut c_void, _reg: WlProxy, name: u32, iface: *const c_char, version: u32) {
    let c = unsafe { &mut *(data as *mut WConn) };
    let s = unsafe { CStr::from_ptr(iface) }.to_str().unwrap_or("");
    let g = Global { name, version };
    match s {
        "wl_compositor" => c.g_comp = Some(g),
        "wl_shm" => c.g_shm = Some(g),
        "wl_seat" => c.g_seat = Some(g),
        "xdg_wm_base" => c.g_wm = Some(g),
        _ => {}
    }
}
unsafe extern "C" fn reg_global_remove(_d: *mut c_void, _r: WlProxy, _n: u32) {}

unsafe extern "C" fn seat_caps(_d: *mut c_void, _s: WlProxy, caps: u32) {
    // WL_SEAT_CAPABILITY_POINTER = 1
    if caps & 1 != 0 {
        let c = unsafe { &mut *(_d as *mut WConn) };
        if c.pointer.is_null() {
            c.pointer = unsafe { (c.f.marshal_ctor)(c.seat, 0, c.f.i_pointer, 1, c.f.i_pointer as *const c_void) };
            let l = Box::new(PointerListener {
                enter: ptr_enter, leave: ptr_leave, motion: ptr_motion,
                button: ptr_button, axis: ptr_axis, _pad: [0; 8],
            });
            unsafe {
                (c.f.add_listener)(c.pointer, &*l as *const PointerListener as *const c_void, c as *mut WConn as *mut c_void);
            }
            std::mem::forget(l);
        }
    }
}
unsafe extern "C" fn seat_name(_d: *mut c_void, _s: WlProxy, _n: *const c_char) {}

fn fixed(v: i32) -> i32 { v >> 8 }

unsafe extern "C" fn ptr_enter(_d: *mut c_void, _p: WlProxy, _serial: u32, _sf: WlProxy, _x: i32, _y: i32) {}
unsafe extern "C" fn ptr_leave(_d: *mut c_void, _p: WlProxy, _s: u32, _sf: WlProxy) {}
unsafe extern "C" fn ptr_motion(d: *mut c_void, _p: WlProxy, _t: u32, x: i32, y: i32) {
    let c = unsafe { &mut *(d as *mut WConn) };
    // no dirty here: events() repaints only when the hover target changes
    c.mouse = (fixed(x), fixed(y));
}
unsafe extern "C" fn ptr_button(d: *mut c_void, _p: WlProxy, _serial: u32, _t: u32, _btn: u32, state: u32) {
    let c = unsafe { &mut *(d as *mut WConn) };
    // WL_POINTER_BUTTON_STATE_PRESSED = 1; only primary button (0x110)
    if state == 1 {
        c.mouse_down = true;
        c.had_press = true;
        c.dirty = true;
    } else {
        c.mouse_down = false;
        if c.had_press {
            c.had_press = false;
            let (x, y) = c.mouse;
            c.click = Some((x, y, true));
        }
        c.dirty = true;
    }
}
unsafe extern "C" fn ptr_axis(_d: *mut c_void, _p: WlProxy, _t: u32, _a: u32, _v: i32) {}

unsafe extern "C" fn wm_ping(d: *mut c_void, _w: WlProxy, serial: u32) {
    let c = unsafe { &mut *(d as *mut WConn) };
    unsafe { (c.f.marshal)(c.wm_base, 3, serial) }; // pong
}

unsafe extern "C" fn xsfc_configure(d: *mut c_void, _s: WlProxy, serial: u32) {
    let c = unsafe { &mut *(d as *mut WConn) };
    c.pending_serial = serial;
    c.have_serial = true;
}

unsafe extern "C" fn xtl_configure(d: *mut c_void, _t: WlProxy, w: i32, h: i32, _states: *mut c_void) {
    let c = unsafe { &mut *(d as *mut WConn) };
    if w > 0 && h > 0 {
        c.configure_w = w;
        c.configure_h = h;
        c.have_configure = true;
        c.dirty = true;
    }
}
unsafe extern "C" fn xtl_close(d: *mut c_void, _t: WlProxy) {
    unsafe { &mut *(d as *mut WConn) }.closed = true;
}
unsafe extern "C" fn xtl_bounds(_d: *mut c_void, _t: WlProxy, _w: i32, _h: i32) {}
unsafe extern "C" fn xtl_caps(_d: *mut c_void, _t: WlProxy, _c: *mut c_void) {}

/* ---- shm buffer management ---- */

const SHM_ARGB8888: u32 = 0;
const SHM_XRGB8888: u32 = 1;

/// wl_buffer "release" listener user data — marks its slot reusable.
struct BufCtx {
    conn: *mut WConn,
    idx: usize,
}

#[repr(C)]
struct BufListener {
    release: unsafe extern "C" fn(*mut c_void, WlProxy),
    _pad: [usize; 4],
}

unsafe extern "C" fn buf_release(d: *mut c_void, _b: WlProxy) {
    let ctx = unsafe { &*(d as *const BufCtx) };
    unsafe { (*ctx.conn).buf_free[ctx.idx] = true };
}

unsafe fn pool_destroy(c: &mut WConn) {
    for b in c.buffers.iter_mut() {
        if !b.is_null() { unsafe { (c.f.marshal)(*b, 0) }; *b = ptr::null_mut(); } // wl_buffer.destroy
    }
    if !c.pool.is_null() {
        unsafe { (c.f.marshal)(c.pool, 1) }; // wl_shm_pool.destroy
        c.pool = ptr::null_mut();
    }
    if !c.shm_data.is_null() {
        unsafe { munmap(c.shm_data as *mut c_void, c.shm_len) };
        c.shm_data = ptr::null_mut();
    }
}

/// (re)create the shm pool + 2 buffers at w*h. Returns false on failure.
unsafe fn pool_create(c: &mut WConn, w: u32, h: u32) -> bool {
    pool_destroy(c);
    let stride = (w * 4) as i32;
    let buf_len = (stride as usize) * h as usize;
    let total = buf_len * 2;
    if c.pool_fd >= 0 { unsafe { close(c.pool_fd) }; c.pool_fd = -1; }
    let fd = unsafe { memfd_create(cs!("zapgui"), 0) };
    if fd < 0 { return false; }
    if unsafe { ftruncate(fd, total as i64) } != 0 { unsafe { close(fd) }; return false; }
    let data = unsafe { mmap(ptr::null_mut(), total, 3, 1, fd, 0) }; // PROT RW, MAP_SHARED
    if data as isize == -1 { unsafe { close(fd) }; return false; }
    c.pool_fd = fd; // stays open until the next pool_create (request already flushed by then)
    c.shm_data = data as *mut u8;
    c.shm_len = total;
    // wl_shm.create_pool(id, fd, size)
    c.pool = unsafe { (c.f.marshal_ctor)(c.shm, 0, c.f.i_shm_pool, 1, c.f.i_shm_pool as *const c_void, fd, total as i32) };
    // fds are dup'd by the compositor; ours can close now — wait, fd already closed above; create_pool needs the fd value while open.
    c.buf_w = w;
    c.buf_h = h;
    for i in 0..2 {
        let off = (i * buf_len) as i32;
        c.buffers[i] = unsafe {
            (c.f.marshal_ctor)(c.pool, 0, c.f.i_buffer, 1, c.f.i_buffer as *const c_void, off, w as i32, h as i32, stride, SHM_XRGB8888)
        };
        // release listener → slot becomes writable again (tiny leak per
        // pool recreate; resize is rare)
        let bl = Box::new(BufListener { release: buf_release, _pad: [0; 4] });
        let ctx = Box::into_raw(Box::new(BufCtx { conn: c as *mut WConn, idx: i }));
        unsafe {
            (c.f.add_listener)(c.buffers[i], &*bl as *const BufListener as *const c_void, ctx as *mut c_void);
        }
        std::mem::forget(bl);
    }
    c.buf_free = [true, true];
    // fresh buffers: shadows invalid, next frame is a full upload
    c.slot_img = [Vec::new(), Vec::new()];
    c.last_slot = usize::MAX;
    true
}

/* ---- open / render / events ---- */

pub fn open(win: &ZUiC, efficient: bool) -> Result<Option<(Box<WConn>, Gui)>, String> {
    let f = match unsafe { wl_load() } {
        Ok(f) => f,
        Err(e) => { return Err(e); }
    };
    let dname = std::env::var("WAYLAND_DISPLAY").ok();
    let dpy = unsafe {
        match dname {
            Some(d) => { let c = CString::new(d).unwrap(); (f.connect)(c.as_ptr()) }
            None => (f.connect)(ptr::null()),
        }
    };
    if dpy.is_null() { return Ok(None); }

    let w = super::prop_i64(win, "width", 800).clamp(1, 8192) as u32;
    let h = super::prop_i64(win, "height", 600).clamp(1, 8192) as u32;

    let mut c = Box::new(WConn {
        f, dpy,
        registry: ptr::null_mut(), compositor: ptr::null_mut(), shm: ptr::null_mut(),
        seat: ptr::null_mut(), pointer: ptr::null_mut(), wm_base: ptr::null_mut(),
        xdg_surface: ptr::null_mut(), xdg_toplevel: ptr::null_mut(), surface: ptr::null_mut(),
        pool: ptr::null_mut(), buffers: [ptr::null_mut(); 2],
        shm_data: ptr::null_mut(), shm_len: 0, buf_w: 0, buf_h: 0, cur: 0,
        g_comp: None, g_shm: None, g_seat: None, g_wm: None,
        closed: false, configure_w: 0, configure_h: 0, have_configure: false, configured: false,
        pending_serial: 0, have_serial: false,
        click: None, had_press: false, dirty: true, buf_free: [true, true],
        efficient, slot_img: [Vec::new(), Vec::new()], last_slot: usize::MAX,
        pool_fd: -1, mouse: (-1, -1), mouse_down: false, text: None,
    });

    c.registry = unsafe { (c.f.marshal_ctor)(c.dpy, 1, c.f.i_registry, 1, c.f.i_registry as *const c_void) }; // wl_display.get_registry
    let rl = Box::new(RegListener { global: reg_global, global_remove: reg_global_remove });
    unsafe {
        (c.f.add_listener)(c.registry, &*rl as *const RegListener as *const c_void, &mut *c as *mut WConn as *mut c_void);
    }
    std::mem::forget(rl);
    unsafe { (c.f.roundtrip)(c.dpy) };

    // required globals
    let (Some(gc), Some(gs), Some(gw)) = (c.g_comp, c.g_shm, c.g_wm) else {
        unsafe { (c.f.disconnect)(c.dpy) };
        return Ok(None);
    };
    let xdg = unsafe { xdg_ifaces(c.f.i_surface) };
    // wl_registry.bind(name, id, interface, version) — sig "usun"
    c.compositor = unsafe { (c.f.marshal_ctor)(c.registry, 0, c.f.i_compositor, gc.version.min(4), gc.name, cs!("wl_compositor"), gc.version.min(4), c.f.i_compositor as *const c_void) };
    c.shm = unsafe { (c.f.marshal_ctor)(c.registry, 0, c.f.i_shm, gs.version.min(1), gs.name, cs!("wl_shm"), gs.version.min(1), c.f.i_shm as *const c_void) };
    c.wm_base = unsafe { (c.f.marshal_ctor)(c.registry, 0, xdg.wm_base, gw.version.min(1), gw.name, cs!("xdg_wm_base"), gw.version.min(1), xdg.wm_base as *const c_void) };
    if let Some(gseat) = c.g_seat {
        let sv = gseat.version.min(1);
        c.seat = unsafe { (c.f.marshal_ctor)(c.registry, 0, c.f.i_seat, sv, gseat.name, cs!("wl_seat"), sv, c.f.i_seat as *const c_void) };
        let sl = Box::new(SeatListener { capabilities: seat_caps, name: seat_name });
        unsafe { (c.f.add_listener)(c.seat, &*sl as *const SeatListener as *const c_void, &mut *c as *mut WConn as *mut c_void) };
        std::mem::forget(sl);
    }

    // surface + xdg toplevel (real server-side titlebar/decorations)
    c.surface = unsafe { (c.f.marshal_ctor)(c.compositor, 0, c.f.i_surface, 4, c.f.i_surface as *const c_void) };
    c.xdg_surface = unsafe { (c.f.marshal_ctor)(c.wm_base, 2, xdg.surface, 1, xdg.surface as *const c_void, c.surface) };
    let xsl = Box::new(XdgSurfaceListener { configure: xsfc_configure, _pad: [0; 4] });
    unsafe { (c.f.add_listener)(c.xdg_surface, &*xsl as *const XdgSurfaceListener as *const c_void, &mut *c as *mut WConn as *mut c_void) };
    std::mem::forget(xsl);
    c.xdg_toplevel = unsafe { (c.f.marshal_ctor)(c.xdg_surface, 1, xdg.toplevel, 1, xdg.toplevel as *const c_void) };
    let tl = Box::new(XdgToplevelListener {
        configure: xtl_configure, close: xtl_close,
        configure_bounds: xtl_bounds, wm_capabilities: xtl_caps,
    });
    unsafe { (c.f.add_listener)(c.xdg_toplevel, &*tl as *const XdgToplevelListener as *mut c_void as *const c_void, &mut *c as *mut WConn as *mut c_void) };
    std::mem::forget(tl);
    let wbl = Box::new(WmBaseListener { ping: wm_ping, _pad: [0; 4] });
    unsafe { (c.f.add_listener)(c.wm_base, &*wbl as *const WmBaseListener as *const c_void, &mut *c as *mut WConn as *mut c_void) };
    std::mem::forget(wbl);

    let title = super::prop_str(win, "title").unwrap_or_else(|| "zap".into());
    let tc = CString::new(title).unwrap();
    let ac = CString::new("zap").unwrap();
    unsafe {
        (c.f.marshal)(c.xdg_toplevel, 2, tc.as_ptr()); // set_title
        (c.f.marshal)(c.xdg_toplevel, 3, ac.as_ptr()); // set_app_id
    }

    // text engine
    c.text = ft::init();

    if !unsafe { pool_create(&mut *c, w, h) } {
        unsafe { (c.f.disconnect)(c.dpy) };
        return Ok(None);
    }

    // bare commit (no buffer) asks the compositor to configure us;
    // buffer commits are gated on configure+ack inside render()
    unsafe { (c.f.marshal)(c.surface, 6) }; // wl_surface.commit
    unsafe { (c.f.flush)(c.dpy) };

    let g = Gui {
        fb_w: w, fb_h: h,
        fb: vec![0xfff2f2f2; (w * h) as usize],
        laid: Vec::new(), texts: Vec::new(),
        mouse: (-1, -1), mouse_down: false, hover: None,
    };
    Ok(Some((c, g)))
}

/// attach current buffer, damage each rect, commit, flip
type Dmg = (i32, i32, i32, i32);

unsafe fn commit(c: &mut WConn, damage: &[Dmg]) {
    let buf = c.buffers[c.cur];
    if buf.is_null() { return; }
    c.last_slot = c.cur;
    (c.f.marshal)(c.surface, 1, buf, 0, 0);       // attach
    for d in damage {
        (c.f.marshal)(c.surface, 2, d.0, d.1, d.2, d.3); // damage
    }
    (c.f.marshal)(c.surface, 6);                  // commit
    c.cur ^= 1;
}

pub fn render(c: &mut WConn, g: &mut Gui, root: *mut ZUiC) {
    // ack any pending configure serial first
    if c.have_serial {
        unsafe { (c.f.marshal)(c.xdg_surface, 4, c.pending_serial) }; // ack_configure
        c.have_serial = false;
        c.configured = true;
    }
    if !c.configured { return; } // wait for first configure before committing
    if c.have_configure {
        let nw = c.configure_w.max(1) as u32;
        let nh = c.configure_h.max(1) as u32;
        c.have_configure = false;
        if nw != g.fb_w || nh != g.fb_h {
            g.fb_w = nw; g.fb_h = nh;
            g.fb = vec![0xfff2f2f2; (nw * nh) as usize];
            unsafe { pool_create(c, nw, nh) };
        }
    }

    // always repaint: the frame pump calls render() every iteration and
    // the user asked for max-fps output, not dirty-rect throttling
    c.dirty = false;
    g.laid.clear();
    g.fb.fill(0xfff2f2f2);
    let engine: &dyn TextOut = match c.text.as_mut() {
        Some(t) => &*t,
        None => &BitmapText,
    };
    super::layout_paint(g, root, engine);
    engine.blit(g);

    // upload fb into a released shm buffer; if the compositor still
    // holds both, drop the frame rather than tear
    let stride = g.fb_w as usize * 4;
    let w = g.fb_w as usize;
    let h = g.fb_h as usize;
    let slot = if c.buf_free[0] { 0 } else if c.buf_free[1] { 1 } else { usize::MAX };
    if slot != usize::MAX && !c.shm_data.is_null() && c.buf_w == g.fb_w && c.buf_h == g.fb_h {
        let off = slot * stride * h;
        if c.efficient {
            efficient_upload(c, g, slot, off, w, h);
        } else {
            let dst = unsafe { c.shm_data.add(off) };
            unsafe {
                for row in 0..h {
                    let s = &g.fb[row * w..(row + 1) * w];
                    ptr::copy_nonoverlapping(s.as_ptr() as *const u8, dst.add(row * stride), stride);
                }
            }
            c.buf_free[slot] = false;
            c.cur = slot;
            unsafe { commit(c, &[(0, 0, g.fb_w as i32, g.fb_h as i32)]) };
            unsafe { (c.f.flush)(c.dpy) };
        }
    }
    g.texts.clear();
}

/// efficientRedraw (§16.2): write only the pixels that differ from this
/// buffer's shadow, and damage only the regions that differ from the
/// last committed frame. A moved element repaints old spot + new spot.
fn efficient_upload(c: &mut WConn, g: &mut Gui, slot: usize, off: usize, w: usize, h: usize) {
    let n = w * h;
    if c.slot_img[slot].len() != n {
        c.slot_img[slot] = vec![0; n];
    }
    let have_last = c.last_slot < 2 && c.slot_img[c.last_slot].len() == n;
    let own = c.slot_img[slot].as_mut_ptr();
    let other = if have_last { c.slot_img[c.last_slot].as_ptr() } else { ptr::null() };
    let fb = g.fb.as_ptr();
    let dst0 = unsafe { c.shm_data.add(off) } as *mut u32;
    let mut dmg_rows: Vec<Vec<(u32, u32)>> = Vec::with_capacity(h);

    unsafe {
        for y in 0..h {
            let row = std::slice::from_raw_parts(fb.add(y * w), w);
            let orow = std::slice::from_raw_parts_mut(own.add(y * w), w);
            let dst = dst0.add(y * w);
            // spans that differ from this buffer's content → shm + shadow
            let mut x = 0;
            while x < w {
                if row[x] != orow[x] {
                    let x0 = x;
                    while x < w && row[x] != orow[x] {
                        x += 1;
                    }
                    ptr::copy_nonoverlapping(row[x0..x].as_ptr(), dst.add(x0), x - x0);
                    orow[x0..x].copy_from_slice(&row[x0..x]);
                } else {
                    x += 1;
                }
            }
            // spans that differ from the last committed frame → damage
            if have_last {
                let prev = std::slice::from_raw_parts(other.add(y * w), w);
                let mut spans: Vec<(u32, u32)> = Vec::new();
                let mut x = 0;
                while x < w {
                    if row[x] != prev[x] {
                        let x0 = x;
                        while x < w && row[x] != prev[x] {
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
                dmg_rows.push(spans);
            }
        }
    }

    // group consecutive rows with identical span sets into damage rects
    let mut damage: Vec<Dmg> = Vec::new();
    if !have_last {
        damage.push((0, 0, w as i32, h as i32));
    } else {
        let mut y0 = 0;
        while y0 < h {
            if dmg_rows[y0].is_empty() {
                y0 += 1;
                continue;
            }
            let spans = &dmg_rows[y0];
            let mut y1 = y0 + 1;
            while y1 < h && dmg_rows[y1] == *spans {
                y1 += 1;
            }
            for &(x0, x1) in spans {
                damage.push((x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32));
            }
            y0 = y1;
        }
        if damage.len() > 64 {
            damage.clear();
            damage.push((0, 0, w as i32, h as i32));
        }
    }

    c.buf_free[slot] = false;
    c.cur = slot;
    unsafe {
        commit(c, &damage);
        (c.f.flush)(c.dpy);
    }
}

pub enum PumpResult { Events(Option<*mut ZUiC>), Close }

/// non-blocking event drain: dispatch everything already queued, then
/// poll the display fd once (0 timeout) and read whatever arrived.
/// Called in a tight loop by the frame pump — the caller repaints every
/// iteration, so this must never block.
fn drain_events(c: &mut WConn) -> bool {
    // events already read into libwayland's buffer
    while unsafe { (c.f.dispatch_pending)(c.dpy) } > 0 {
        if c.closed { return false; }
    }
    // socket readable? single-shot prepare→read→dispatch. The display fd
    // is nonblocking: looping read_events here spins on EAGAIN forever.
    let fd = unsafe { (c.f.get_fd)(c.dpy) };
    let mut pfd = [PollFd { fd, events: POLLIN, revents: 0 }];
    if unsafe { poll(pfd.as_mut_ptr(), 1, 0) } <= 0 {
        return true;
    }
    if pfd[0].revents & POLLNVAL != 0 {
        return false;
    }
    if unsafe { (c.f.prepare_read)(c.dpy) } == 0 {
        // EAGAIN races between poll and read are harmless — just means
        // another reader got there first; real errors surface via dispatch
        let _ = unsafe { (c.f.read_events)(c.dpy) };
        while unsafe { (c.f.dispatch_pending)(c.dpy) } > 0 {
            if c.closed { return false; }
        }
    }
    true
}

/// pump pending events once (never blocks), then report what changed
pub fn events(c: &mut WConn, g: &mut Gui) -> PumpResult {
    g.mouse = c.mouse;
    g.mouse_down = c.mouse_down;
    if !drain_events(c) {
        return PumpResult::Close;
    }
    g.mouse = c.mouse;
    g.mouse_down = c.mouse_down;
    let hov = super::hit_test(g, g.mouse.0, g.mouse.1);
    if hov != g.hover {
        g.hover = hov;
        c.dirty = true;
    }
    let mut clicked = None;
    if let Some((x, y, pressed)) = c.click.take() {
        g.mouse = (x, y);
        if pressed {
            // hit test against last layout, topmost first
            for l in g.laid.iter().rev() {
                let r = l.r;
                if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
                    clicked = Some(l.el);
                    break;
                }
            }
        }
    }
    PumpResult::Events(clicked)
}

/* ================= freetype text engine ================= */

mod ft {
    use super::*;

    #[repr(C)]
    struct FtBitmap {
        rows: u32, width: u32, pitch: i32, buffer: *mut u8,
        num_grays: u16, pixel_mode: u8, _pad: [u8; 5], palette: *mut c_void,
    }

    // offsets into opaque FT_FaceRec / FT_GlyphSlotRec (LP64, freetype 2.x stable ABI)
    const FACE_GLYPH: usize = 152;
    const SLOT_ADV_X: usize = 128;   // FT_Vector.x, 26.6
    const SLOT_METRICS: usize = 48;  // FT_Glyph_Metrics; horiAdvance @ +32 (26.6)
    const SLOT_BITMAP: usize = 152;  // FT_Bitmap (40 bytes)
    const SLOT_BMP_LEFT: usize = 192;
    const SLOT_BMP_TOP: usize = 196;
    const FACE_SIZE: usize = 160;    // FT_Size ptr
    const SIZE_METRICS: usize = 24;  // FT_Size_Metrics within FT_SizeRec
    const MET_ASC: usize = 24;       // ascender (26.6)
    const MET_DESC: usize = 32;      // descender (26.6)
    const MET_HEIGHT: usize = 40;    // height (26.6)

    #[derive(Clone)]
    struct Glyph {
        w: i32, h: i32, left: i32, top: i32, adv: i32,
        buf: Vec<u8>, // coverage, row-major, pitch may be negative → normalized
    }

    pub struct Ft {
        lib: *mut c_void,
        faces: RefCell<BTreeMap<(String, i32), (*mut c_void, i32, i32)>>, // face, asc, line_h
        glyphs: RefCell<BTreeMap<(*mut c_void, u32), Option<Glyph>>>,
        f: Box<FFn>,
    }

    struct FFn {
        init: unsafe extern "C" fn(*mut *mut c_void) -> u32,
        done_lib: unsafe extern "C" fn(*mut c_void) -> u32,
        new_face: unsafe extern "C" fn(*mut c_void, *const c_char, i64, *mut *mut c_void) -> u32,
        done_face: unsafe extern "C" fn(*mut c_void) -> u32,
        set_px: unsafe extern "C" fn(*mut c_void, u32, u32) -> u32,
        load_char: unsafe extern "C" fn(*mut c_void, u64, i32) -> u32,
        char_index: unsafe extern "C" fn(*mut c_void, u64) -> u32,
        // fontconfig
        fc_init: unsafe extern "C" fn() -> c_int,
        fc_init_load: unsafe extern "C" fn() -> c_int,
        fc_config: unsafe extern "C" fn() -> *mut c_void,
        fc_patbuild: unsafe extern "C" fn(*mut c_void, *const c_char, ...) -> *mut c_void,
        fc_fontmatch: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_int) -> *mut c_void,
        fc_patdestroy: unsafe extern "C" fn(*mut c_void) -> c_int,
        fc_getstr: unsafe extern "C" fn(*mut c_void, *const c_char, u32, *mut *mut c_char) -> c_int,
    }

    pub fn init() -> Option<Ft> {
        unsafe {
            let fl = dlopen(cs!("libfreetype.so.6"), 2);
            let fcl = dlopen(cs!("libfontconfig.so.1"), 2);
            if fl.is_null() || fcl.is_null() { return None; }
            macro_rules! s { ($lib:expr, $n:literal, $t:ty) => { match wsym::<$t>($lib, $n) { Ok(v) => v, Err(()) => return None } } }
            let f = FFn {
                init: s!(fl, "FT_Init_FreeType", unsafe extern "C" fn(*mut *mut c_void) -> u32),
                done_lib: s!(fl, "FT_Done_FreeType", unsafe extern "C" fn(*mut c_void) -> u32),
                new_face: s!(fl, "FT_New_Face", unsafe extern "C" fn(*mut c_void, *const c_char, i64, *mut *mut c_void) -> u32),
                done_face: s!(fl, "FT_Done_Face", unsafe extern "C" fn(*mut c_void) -> u32),
                set_px: s!(fl, "FT_Set_Pixel_Sizes", unsafe extern "C" fn(*mut c_void, u32, u32) -> u32),
                load_char: s!(fl, "FT_Load_Char", unsafe extern "C" fn(*mut c_void, u64, i32) -> u32),
                char_index: s!(fl, "FT_Get_Char_Index", unsafe extern "C" fn(*mut c_void, u64) -> u32),
                fc_init: s!(fcl, "FcInit", unsafe extern "C" fn() -> c_int),
                fc_init_load: s!(fcl, "FcInitLoadConfig", unsafe extern "C" fn() -> c_int),
                fc_config: s!(fcl, "FcConfigGetCurrent", unsafe extern "C" fn() -> *mut c_void),
                fc_patbuild: s!(fcl, "FcPatternBuild", unsafe extern "C" fn(*mut c_void, *const c_char, ...) -> *mut c_void),
                fc_fontmatch: s!(fcl, "FcFontMatch", unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_int) -> *mut c_void),
                fc_patdestroy: s!(fcl, "FcPatternDestroy", unsafe extern "C" fn(*mut c_void) -> c_int),
                fc_getstr: s!(fcl, "FcPatternGetString", unsafe extern "C" fn(*mut c_void, *const c_char, u32, *mut *mut c_char) -> c_int),
            };
            let mut lib: *mut c_void = ptr::null_mut();
            if (f.init)(&mut lib) != 0 || lib.is_null() { return None; }
            (f.fc_init)();
            (f.fc_init_load)();
            Some(Ft { lib, faces: RefCell::new(BTreeMap::new()), glyphs: RefCell::new(BTreeMap::new()), f: Box::new(f) })
        }
    }

    impl Ft {
        /// resolve family+px → (face, ascender_px, line_height_px)
        fn face_for(&self, fam: &str, px: i32) -> Option<(*mut c_void, i32, i32)> {
            let key = (fam.to_string(), px);
            if let Some(&v) = self.faces.borrow().get(&key) { return Some(v); }
            let path = unsafe { self.font_path(fam, px) }?;
            let pc = CString::new(path).ok()?;
            let mut face: *mut c_void = ptr::null_mut();
            unsafe {
                let ff = &*self.f;
                if (ff.new_face)(self.lib, pc.as_ptr(), 0, &mut face) != 0 || face.is_null() {
                    return None;
                }
                (ff.set_px)(face, 0, px.max(6) as u32);
                // metrics via face->size->metrics (26.6 fixed)
                let size = *(face.add(FACE_SIZE) as *const *mut u8);
                let (mut asc, mut lh) = (px * 3 / 4, px + px / 4);
                if !size.is_null() {
                    let m = size.add(SIZE_METRICS);
                    asc = (*(m.add(MET_ASC) as *const i64) >> 6) as i32;
                    let desc = (*(m.add(MET_DESC) as *const i64) >> 6) as i32;
                    lh = (*(m.add(MET_HEIGHT) as *const i64) >> 6) as i32;
                    if lh <= 0 { lh = asc - desc; }
                }
                let v = (face, asc.max(1), lh.max(px / 2));
                self.faces.borrow_mut().insert(key, v);
                Some(v)
            }
        }

        unsafe fn font_path(&self, fam: &str, px: i32) -> Option<String> {
            let ff = &*self.f;
            let cfg = (ff.fc_config)();
            if cfg.is_null() { return None; }
            let famc = CString::new(fam).unwrap_or_else(|_| CString::new("DejaVu Sans").unwrap());
            let pat = (ff.fc_patbuild)(ptr::null_mut(),
                cs!("family"), 3, famc.as_ptr(),
                cs!("pixelsize"), 2, px as f64,
                ptr::null::<c_char>());
            if pat.is_null() { return None; }
            let mut res: c_int = 0;
            let m = (ff.fc_fontmatch)(cfg, pat, &mut res);
            (ff.fc_patdestroy)(pat);
            if m.is_null() { return None; }
            let mut sp: *mut c_char = ptr::null_mut();
            let ok = (ff.fc_getstr)(m, cs!("file"), 0, &mut sp);
            let out = if ok == 0 && !sp.is_null() {
                CStr::from_ptr(sp).to_str().ok().map(|s| s.to_string())
            } else { None };
            (ff.fc_patdestroy)(m);
            out
        }

        /// render (and cache) one glyph; returns None for .notdef/empty
        unsafe fn glyph(&self, face: *mut c_void, ch: u32) -> Option<Glyph> {
            let key = (face, ch);
            if let Some(g) = self.glyphs.borrow().get(&key) { return g.clone(); }
            let ff = &*self.f;
            let idx = (ff.char_index)(face, ch as u64);
            let mut out: Option<Glyph> = None;
            if idx != 0 && (ff.load_char)(face, ch as u64, 4) == 0 { // FT_LOAD_RENDER
                let slot = *(face.add(FACE_GLYPH) as *const *mut u8);
                if !slot.is_null() {
                    let bm = slot.add(SLOT_BITMAP);
                    let rows = *(bm as *const u32);
                    let w = *(bm.add(4) as *const u32);
                    let pitch = *(bm.add(8) as *const i32);
                    let bufp = *(bm.add(16) as *const *mut u8);
                    let left = *(slot.add(SLOT_BMP_LEFT) as *const i32);
                    let top = *(slot.add(SLOT_BMP_TOP) as *const i32);
                    let adv = (*(slot.add(SLOT_ADV_X) as *const i64) + 32) >> 6;
                    if w > 0 && rows > 0 && !bufp.is_null() {
                        let pw = pitch.unsigned_abs() as usize;
                        let mut data = vec![0u8; (w * rows) as usize];
                        for r in 0..rows as usize {
                            let src = if pitch >= 0 { bufp.add(r * pw) } else { bufp.add((rows as usize - 1 - r) * pw) };
                            ptr::copy_nonoverlapping(src, data.as_mut_ptr().add(r * w as usize), w as usize);
                        }
                        out = Some(Glyph { w: w as i32, h: rows as i32, left, top, adv: adv as i32, buf: data });
                    } else {
                        out = Some(Glyph { w: 0, h: 0, left: 0, top: 0, adv: adv as i32, buf: Vec::new() });
                    }
                }
            }
            self.glyphs.borrow_mut().insert(key, out.clone());
            out
        }

        fn measure_ft(&self, fam: &str, px: i32, s: &str) -> (i32, i32) {
            match self.face_for(fam, px) {
                Some((face, _asc, lh)) => {
                    let mut w = 0i32;
                    for ch in s.chars() {
                        if let Some(g) = unsafe { self.glyph(face, ch as u32) } { w += g.adv; }
                    }
                    (w, lh)
                }
                None => super::BitmapText.measure(fam, px, s),
            }
        }

        /// blend glyph coverage into fb at pen position
        fn blit_glyph(g: &mut Gui, gl: &Glyph, pen_x: i32, base_y: i32, color: u32) {
            let (cr, cg, cb) = ((color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff);
            let x0 = pen_x + gl.left;
            let y0 = base_y - gl.top;
            for r in 0..gl.h {
                let dy = y0 + r;
                if dy < 0 || dy >= g.fb_h as i32 { continue; }
                for col in 0..gl.w {
                    let dx = x0 + col;
                    if dx < 0 || dx >= g.fb_w as i32 { continue; }
                    let a = gl.buf[(r * gl.w + col) as usize] as u32;
                    if a == 0 { continue; }
                    if a == 255 {
                        g.fb[dy as usize * g.fb_w as usize + dx as usize] = 0xff00_0000 | color;
                        continue;
                    }
                    let i = dy as usize * g.fb_w as usize + dx as usize;
                    let d = g.fb[i];
                    let dr = (d >> 16) & 0xff;
                    let dg = (d >> 8) & 0xff;
                    let db = d & 0xff;
                    let inv = 255 - a;
                    let nr = (cr * a + dr * inv + 127) / 255;
                    let ng = (cg * a + dg * inv + 127) / 255;
                    let nb = (cb * a + db * inv + 127) / 255;
                    g.fb[i] = 0xff00_0000 | (nr << 16) | (ng << 8) | nb;
                }
            }
        }
    }

    impl TextOut for Ft {
        fn external(&self) -> bool { true }
        fn line_h(&self, px: i32) -> i32 { px + px / 4 }
        fn measure(&self, fam: &str, px: i32, s: &str) -> (i32, i32) {
            self.measure_ft(fam, px, s)
        }
        fn blit(&self, g: &mut Gui) {
            let ops = std::mem::take(&mut g.texts);
            for op in &ops {
                let Some((face, _asc, _lh)) = self.face_for(&op.fam, op.px) else { continue };
                let mut pen = op.x;
                let base = op.y; // TextOp.y is already the baseline
                for ch in op.s.chars() {
                    if let Some(gl) = unsafe { self.glyph(face, ch as u32) } {
                        Ft::blit_glyph(g, &gl, pen, base, op.color);
                        pen += gl.adv;
                    }
                }
            }
            g.texts = ops;
        }
    }
}
