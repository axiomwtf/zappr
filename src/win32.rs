//! Win32/GDI backend for zapgui (SPEC §16). Compiled only for
//! windows targets; all Win32 imports resolve through the windows-gnu
//! std import libraries. Software ARGB framebuffer in a top-down
//! 32bpp DIB section, GDI text drawn into the same DC.

use core::ffi::c_void;
use core::ptr;
use core::cell::RefCell;
use core::sync::atomic::{AtomicPtr, Ordering};
use super::{Gui, TextOp, TextOut, ZUiC};
use super::{prop_i64, prop_str, layout_paint};

/* ---------- Win32 FFI ---------- */

type Hwnd = *mut c_void;
type Hdc = *mut c_void;
type Hbitmap = *mut c_void;
type Hfont = *mut c_void;
type Hgdiobj = *mut c_void;
type Hinstance = *mut c_void;
type Hcursor = *mut c_void;
type Hicon = *mut c_void;
type Hmenu = *mut c_void;
type Wparam = usize;
type Lparam = isize;
type Lresult = isize;
type Bool = i32;

#[link(name = "gdi32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Hinstance;
    fn RegisterClassW(cls: *const WndClassW) -> u16;
    fn CreateWindowExW(exstyle: u32, class: *const u16, title: *const u16,
        style: u32, x: i32, y: i32, w: i32, h: i32,
        parent: Hwnd, menu: Hmenu, inst: Hinstance, param: *mut c_void) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, w: Wparam, l: Lparam) -> Lresult;
    fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> Bool;
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> Bool;
    fn TranslateMessage(msg: *const Msg) -> Bool;
    fn DispatchMessageW(msg: *const Msg) -> Lresult;
    fn PostQuitMessage(code: i32);
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> Bool;
    fn UpdateWindow(hwnd: Hwnd) -> Bool;
    fn GetDC(hwnd: Hwnd) -> Hdc;
    fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
    fn SetWindowLongPtrW(hwnd: Hwnd, idx: i32, val: isize) -> isize;
    fn BeginPaint(hwnd: Hwnd, ps: *mut PaintStruct) -> Hdc;
    fn EndPaint(hwnd: Hwnd, ps: *const PaintStruct) -> Bool;
    fn LoadCursorW(inst: Hinstance, name: usize) -> Hcursor;

    fn CreateDIBSection(hdc: Hdc, bmi: *const BitmapInfo, usage: u32,
        bits: *mut *mut c_void, section: Hinstance, offset: u32) -> Hbitmap;
    fn CreateCompatibleDC(hdc: Hdc) -> Hdc;
    fn SelectObject(hdc: Hdc, obj: Hgdiobj) -> Hgdiobj;
    fn DeleteObject(obj: Hgdiobj) -> Bool;
    fn DeleteDC(hdc: Hdc) -> Bool;
    fn BitBlt(dst: Hdc, x: i32, y: i32, w: i32, h: i32,
        src: Hdc, sx: i32, sy: i32, rop: u32) -> Bool;
    fn CreateFontW(h: i32, w: i32, esc: i32, orient: i32, weight: i32,
        italic: u32, underline: u32, strike: u32, charset: i32,
        outprec: i32, clipprec: i32, quality: i32, pitch: i32,
        face: *const u16) -> Hfont;
    fn SetBkMode(hdc: Hdc, mode: i32) -> i32;
    fn SetTextColor(hdc: Hdc, color: u32) -> u32;
    fn ExtTextOutW(hdc: Hdc, x: i32, y: i32, opts: u32, rect: *const RectW,
        s: *const u16, count: u32, dx: *const i32) -> Bool;
    fn GetTextExtentPoint32W(hdc: Hdc, s: *const u16, count: i32,
        size: *mut SizeW) -> Bool;
}

#[repr(C)]
struct WndClassW {
    style: u32,
    wndproc: unsafe extern "system" fn(Hwnd, u32, Wparam, Lparam) -> Lresult,
    cls_extra: i32,
    wnd_extra: i32,
    inst: Hinstance,
    icon: Hicon,
    cursor: Hcursor,
    background: Hgdiobj,
    menu: *const u16,
    class: *const u16,
    icon_sm: Hicon,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: Wparam,
    lparam: Lparam,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RectW { left: i32, top: i32, right: i32, bottom: i32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SizeW { cx: i32, cy: i32 }

#[repr(C)]
struct PaintStruct {
    hdc: Hdc,
    erase: Bool,
    rc_paint: RectW,
    restore: Bool,
    inc_update: Bool,
    reserved: [u32; 8],
}

#[repr(C)]
struct BitmapInfoHeader {
    size: u32, width: i32, height: i32, planes: u16, bit_count: u16,
    compression: u32, size_image: u32, x_ppm: i32, y_ppm: i32,
    clr_used: u32, clr_important: u32,
}

#[repr(C)]
struct BitmapInfo { header: BitmapInfoHeader, colors: [u32; 3] }

const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const WS_VISIBLE: u32 = 0x0100_0000;
const CW_USEDEFAULT: i32 = 0x8000_0000u32 as i32;
const GWLP_USERDATA: i32 = -21;
const WM_DESTROY: u32 = 0x0002;
const WM_SIZE: u32 = 0x0005;
const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_MOUSEACTIVATE: u32 = 0x0021;
const SRCCOPY: u32 = 0x00CC_0020;
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;
const TRANSPARENT: i32 = 1;
const ANTIALIASED_QUALITY: i32 = 4;
const FW_NORMAL: i32 = 400;
const DEFAULT_CHARSET: i32 = 1;
const OUT_DEFAULT_PRECIS: i32 = 0;
const CLIP_DEFAULT_PRECIS: i32 = 0;
const VARIABLE_PITCH: i32 = 0;
const FF_DONTCARE: i32 = 0;
const IDC_ARROW: usize = 32512;
const MA_ACTIVATE: Lresult = 1;

fn w16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

fn lo_word(v: Lparam) -> i32 { (v as u32 & 0xffff) as u16 as i16 as i32 }
fn hi_word(v: Lparam) -> i32 { ((v as u32 >> 16) as u16) as i16 as i32 }

/* ---------- connection ---------- */

static CONN: AtomicPtr<WinConn> = AtomicPtr::new(ptr::null_mut());

pub struct WinConn {
    pub hwnd: Hwnd,
    pub memdc: Hdc,
    pub dib: Hbitmap,
    pub old_obj: Hgdiobj,
    pub bits: *mut u8,
    pub w: i32,
    pub h: i32,
    pub stride: i32,
    pub fonts: RefCell<Vec<(String, i32, Hfont)>>,
    pub mouse: (i32, i32),
    pub mouse_down: bool,
    pub had_press: bool,
    pub click: Option<(i32, i32, bool)>,
    pub closed: bool,
    pub pending_size: Option<(i32, i32)>,
    /// efficientRedraw: last presented frame for damage diffs
    pub efficient: bool,
    pub prev: Vec<u32>,
    root: *mut ZUiC,
}

unsafe fn dib_create(c: &mut WinConn, w: i32, h: i32) -> bool {
    if !c.dib.is_null() {
        let _ = DeleteObject(c.dib);
        c.dib = ptr::null_mut();
    }
    let stride = (w * 4 + 3) & !3;
    let bmi = BitmapInfo { header: BitmapInfoHeader {
        size: 40, width: w, height: -h, planes: 1, bit_count: 32,
        compression: BI_RGB, size_image: 0, x_ppm: 0, y_ppm: 0,
        clr_used: 0, clr_important: 0,
    }, colors: [0; 3] };
    let mut bits: *mut c_void = ptr::null_mut();
    let dib = CreateDIBSection(c.memdc, &bmi, DIB_RGB_COLORS,
        &mut bits, ptr::null_mut(), 0);
    if dib.is_null() { return false; }
    c.dib = dib;
    c.bits = bits as *mut u8;
    c.stride = stride;
    ptr::write_bytes(c.bits, 0, (stride * h) as usize);
    true
}

fn rgb_to_colorref(c: u32) -> u32 {
    ((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff)
}

/* ---------- window proc ---------- */

unsafe extern "system" fn wp(hwnd: Hwnd, msg: u32, w: Wparam, l: Lparam) -> Lresult {
    unsafe {
        let cp = CONN.load(Ordering::Relaxed);
        if cp.is_null() { return DefWindowProcW(hwnd, msg, w, l); }
        let c = &mut *cp;
        match msg {
            WM_SIZE => {
                let nw = lo_word(l);
                let nh = hi_word(l);
                if nw > 0 && nh > 0 { c.pending_size = Some((nw, nh)); }
                0
            }
            WM_MOUSEMOVE => {
                c.mouse = (lo_word(l), hi_word(l));
                0
            }
            WM_LBUTTONDOWN => {
                c.mouse = (lo_word(l), hi_word(l));
                c.mouse_down = true;
                c.had_press = true;
                0
            }
            WM_LBUTTONUP => {
                c.mouse = (lo_word(l), hi_word(l));
                c.mouse_down = false;
                if c.had_press {
                    c.had_press = false;
                    c.click = Some((c.mouse.0, c.mouse.1, true));
                }
                0
            }
            WM_MOUSEACTIVATE => MA_ACTIVATE,
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps = PaintStruct { hdc: ptr::null_mut(), erase: 0,
                    rc_paint: RectW::default(), restore: 0, inc_update: 0, reserved: [0; 8] };
                let hdc = BeginPaint(hwnd, &mut ps);
                let _ = BitBlt(hdc, 0, 0, c.w, c.h, c.memdc, 0, 0, SRCCOPY);
                let _ = EndPaint(hwnd, &ps);
                0
            }
            WM_DESTROY => {
                c.closed = true;
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// blit DIB contents to a window dc
unsafe fn present(c: &WinConn, dst: Hdc) {
    unsafe { let _ = BitBlt(dst, 0, 0, c.w, c.h, c.memdc, 0, 0, SRCCOPY); }
}

/// efficientRedraw: blit only the damage rects to the window dc
unsafe fn present_damage(c: &WinConn, dst: Hdc, rects: &[(u32, u32, u32, u32)]) {
    for &(x, y, w, h) in rects {
        unsafe {
            let _ = BitBlt(dst, x as i32, y as i32, w as i32, h as i32,
                c.memdc, x as i32, y as i32, SRCCOPY);
        }
    }
}

/* ---------- GDI text engine ---------- */

pub struct GdiText<'a> {
    hdc: Hdc,
    fonts: &'a RefCell<Vec<(String, i32, Hfont)>>,
}

impl<'a> GdiText<'a> {
    fn font_for(&self, fam: &str, px: i32) -> Hfont {
        unsafe {
            let mut fonts = self.fonts.borrow_mut();
            for (f, p, h) in fonts.iter() {
                if f == fam && *p == px { return *h; }
            }
            let face = w16(fam);
            let hf = CreateFontW(-px, 0, 0, 0, FW_NORMAL, 0, 0, 0,
                DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY, VARIABLE_PITCH | FF_DONTCARE, face.as_ptr());
            if !hf.is_null() { fonts.push((fam.to_string(), px, hf)); }
            hf
        }
    }
}

impl TextOut for GdiText<'_> {
    fn external(&self) -> bool { true }
    fn line_h(&self, px: i32) -> i32 { px + px / 3 }
    fn measure(&self, fam: &str, px: i32, s: &str) -> (i32, i32) {
        let hf = self.font_for(fam, px);
        if hf.is_null() {
            return ((s.chars().count() as i32 * px * 6) / 10, px + px / 3);
        }
        unsafe {
            let old = SelectObject(self.hdc, hf as Hgdiobj);
            let wtxt = w16(s);
            let mut sz = SizeW::default();
            let ok = GetTextExtentPoint32W(self.hdc, wtxt.as_ptr(),
                s.chars().count() as i32, &mut sz);
            let _ = SelectObject(self.hdc, old);
            if ok != 0 { (sz.cx, sz.cy) } else { (s.len() as i32 * px / 2, px + px / 3) }
        }
    }
    fn blit(&self, g: &mut Gui) {
        unsafe {
            for TextOp { x, y, s, color, fam, px } in g.texts.drain(..) {
                let hf = self.font_for(&fam, px);
                if !hf.is_null() {
                    let _ = SelectObject(self.hdc, hf as Hgdiobj);
                }
                let _ = SetTextColor(self.hdc, rgb_to_colorref(color));
                let wtxt = w16(&s);
                let _ = ExtTextOutW(self.hdc, x, y, 0, ptr::null(),
                    wtxt.as_ptr(), s.chars().count() as u32, ptr::null());
            }
        }
    }
}

/* ---------- backend interface (mirrors wayland.rs) ---------- */

pub enum PumpResult { Events(Option<*mut ZUiC>), Close }

pub fn open(win: &ZUiC, efficient: bool) -> Result<Option<(Box<WinConn>, Gui)>, String> {
    unsafe {
        let w = prop_i64(win, "width", 800).clamp(1, 8192) as i32;
        let h = prop_i64(win, "height", 600).clamp(1, 8192) as i32;
        let inst = GetModuleHandleW(ptr::null());
        if inst.is_null() { return Ok(None); }

        let class = w16("zapgui");
        let wc = WndClassW {
            style: 0,
            wndproc: wp,
            cls_extra: 0,
            wnd_extra: 0,
            inst,
            icon: ptr::null_mut(),
            cursor: LoadCursorW(inst, IDC_ARROW),
            background: ptr::null_mut(),
            menu: ptr::null(),
            class: class.as_ptr(),
            icon_sm: ptr::null_mut(),
        };
        RegisterClassW(&wc);

        let title = prop_str(win, "title").unwrap_or_else(|| "zap".to_string());
        let tw = w16(&title);
        let hwnd = CreateWindowExW(0, class.as_ptr(), tw.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT, CW_USEDEFAULT, w, h,
            ptr::null_mut(), ptr::null_mut(), inst, ptr::null_mut());
        if hwnd.is_null() { return Ok(None); }

        let screen = GetDC(ptr::null_mut());
        let memdc = CreateCompatibleDC(screen);
        let _ = ReleaseDC(ptr::null_mut(), screen);
        if memdc.is_null() { return Ok(None); }

        let mut c = Box::new(WinConn {
            hwnd, memdc, dib: ptr::null_mut(), old_obj: ptr::null_mut(),
            bits: ptr::null_mut(), w, h, stride: 0,
            fonts: RefCell::new(Vec::new()),
            mouse: (-1, -1), mouse_down: false, had_press: false,
            click: None, closed: false, pending_size: None,
            efficient, prev: Vec::new(),
            root: win as *const ZUiC as *mut ZUiC,
        });
        CONN.store(&mut *c, Ordering::Relaxed);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *c as *mut WinConn as isize);

        if !dib_create(&mut c, w, h) {
            let _ = DeleteDC(memdc);
            return Ok(None);
        }
        c.old_obj = SelectObject(memdc, c.dib);
        let _ = SetBkMode(memdc, TRANSPARENT);
        let _ = UpdateWindow(hwnd);

        let g = Gui {
            fb_w: w as u32, fb_h: h as u32,
            fb: vec![0xfff2f2f2; (w * h) as usize],
            laid: Vec::new(), texts: Vec::new(),
            mouse: (-1, -1), mouse_down: false, hover: None,
        };
        Ok(Some((c, g)))
    }
}

fn blit_fb(c: &WinConn, g: &Gui) {
    unsafe {
        let stride = c.stride as usize;
        for row in 0..g.fb_h as usize {
            let s = &g.fb[row * g.fb_w as usize..(row + 1) * g.fb_w as usize];
            ptr::copy_nonoverlapping(s.as_ptr() as *const u8,
                c.bits.add(row * stride), g.fb_w as usize * 4);
        }
    }
}

pub fn render(c: &mut WinConn, g: &mut Gui, root: *mut ZUiC) {
    if let Some((nw, nh)) = c.pending_size.take() {
        unsafe {
            if dib_create(c, nw, nh) {
                let _ = SelectObject(c.memdc, c.dib);
                c.w = nw; c.h = nh;
                g.fb_w = nw as u32; g.fb_h = nh as u32;
                g.fb = vec![0xfff2f2f2; (nw * nh) as usize];
            }
        }
    }
    let engine = GdiText { hdc: c.memdc, fonts: &c.fonts };
    layout_paint(g, root, &engine);
    engine.blit(g);
    blit_fb(c, g);
    unsafe {
        let dst = GetDC(c.hwnd);
        if !dst.is_null() {
            if c.efficient {
                let w = g.fb_w as usize;
                let h = g.fb_h as usize;
                if c.prev.len() != w * h {
                    c.prev = vec![0; w * h];
                }
                let damage = super::diff_damage(&g.fb, &c.prev, w, h);
                present_damage(c, dst, &damage);
                c.prev.clear();
                c.prev.extend_from_slice(&g.fb);
            } else {
                present(c, dst);
            }
            let _ = ReleaseDC(c.hwnd, dst);
        }
    }
}

/// pump messages until a click or close; repaint on state changes
pub fn events(c: &mut WinConn, g: &mut Gui) -> PumpResult {
    unsafe {
        let mut msg = Msg { hwnd: ptr::null_mut(), message: 0, wparam: 0,
            lparam: 0, time: 0, pt_x: 0, pt_y: 0 };
        loop {
            // drain the queue without blocking — the frame pump repaints
            // every iteration, so this must return immediately when idle
            let r = PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, 1);
            if r <= 0 { break; }
            if msg.message == 0x0012 {
                return PumpResult::Close; // WM_QUIT
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
            if c.closed { return PumpResult::Close; }
        }

        g.mouse = c.mouse;
        g.mouse_down = c.mouse_down;
        if let Some((x, y, pressed)) = c.click.take() {
            g.mouse = (x, y);
            if pressed {
                let mut clicked = None;
                for l in g.laid.iter().rev() {
                    let r = l.r;
                    if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
                        clicked = Some(l.el);
                        break;
                    }
                }
                return PumpResult::Events(clicked);
            }
        }
        PumpResult::Events(None)
    }
}
