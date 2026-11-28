//! In-process GUI bridge: drives the native renderer (lib.rs Wayland /
//! Win32 backends) straight from the interpreter's UiElement tree —
//! no compiled artifact, no C runtime (§16).
//!
//! The renderer speaks the ZUiC/ZValC C ABI; we build that tree here
//! from `Rc<RefCell<UiElement>>` and marshal click handlers back into
//! `Vm::call_func_value` through a side table.

use crate::interp::Vm;
use crate::uiruntime::UiElement;
use crate::value::{FuncVal, Value};
use std::cell::RefCell;
use std::ffi::CString;
use std::rc::Rc;

use zapgui::{
    zap_gui_run, ZInvokeCb, ZPairC, ZRefreshCb, ZUiC, ZValC, ZValU, ZVecC, Z_BOOL, Z_BYTES,
    Z_FLOAT, Z_FUNC, Z_INT, Z_LIST, Z_NOTHING, Z_STR,
};

/// The Vm currently inside `ui.run` — set for the duration of the
/// native event loop so click handlers can re-enter the interpreter.
static mut HOST_VM: *mut Vm = std::ptr::null_mut();
thread_local! {
    /// The live UiElement window being rendered; refresh re-marshals it.
    static HOST_EL: RefCell<Option<Rc<RefCell<UiElement>>>> = const { RefCell::new(None) };
}
/// Most recently handed-out ZUiC tree — freed on the next refresh.
static mut LAST_TREE: *mut ZUiC = std::ptr::null_mut();

/// Click-handler payload stored behind a Z_FUNC's opaque fn pointer.
struct HostFn {
    f: Rc<FuncVal>,
}

fn cstr(s: &str) -> *mut std::os::raw::c_char {
    CString::new(s).unwrap_or_default().into_raw()
}

fn val_to_zvalc(v: &Value) -> ZValC {
    let mut z = ZValC {
        tag: Z_NOTHING,
        _pad: 0,
        aux: 0,
        u: ZValU { i: 0 },
    };
    match v {
        Value::Int(i) => {
            z.tag = Z_INT;
            z.u.i = *i as i64;
        }
        Value::Float(f) => {
            z.tag = Z_FLOAT;
            z.u.f = *f;
        }
        Value::Str(s) => {
            z.tag = Z_STR;
            z.u.s = cstr(&s.borrow());
        }
        Value::Bool(b) => {
            z.tag = Z_BOOL;
            z.u.b = *b as i32;
        }
        Value::Func(f) => {
            z.tag = Z_FUNC;
            z.u.fnp = Box::into_raw(Box::new(HostFn { f: f.clone() })) as *mut std::ffi::c_void;
        }
        Value::List(l) => {
            z.tag = Z_LIST;
            let items: Vec<ZValC> = l.borrow().iter().map(val_to_zvalc).collect();
            let n = items.len();
            let ptr = if n > 0 {
                let mut b = items.into_boxed_slice();
                let p = b.as_mut_ptr();
                std::mem::forget(b);
                p
            } else {
                std::ptr::null_mut()
            };
            z.u.vec = Box::into_raw(Box::new(ZVecC { v: ptr, n, cap: n }));
        }
        _ => {}
    }
    z
}

fn zstr(s: &str) -> ZValC {
    ZValC {
        tag: Z_STR,
        _pad: 0,
        aux: 0,
        u: ZValU { s: cstr(s) },
    }
}

fn prop_to_pair(name: &str, v: &Value) -> ZPairC {
    ZPairC {
        k: zstr(name),
        v: val_to_zvalc(v),
    }
}

fn el_to_zui(el: &Rc<RefCell<UiElement>>) -> *mut ZUiC {
    let e = el.borrow();
    let props: Vec<ZPairC> = e.props.iter().map(|(n, v)| prop_to_pair(n, v)).collect();
    let pn = props.len();
    let props_ptr = if pn > 0 {
        let mut b = props.into_boxed_slice();
        let p = b.as_mut_ptr();
        std::mem::forget(b);
        p
    } else {
        std::ptr::null_mut()
    };
    let children: Vec<*mut ZUiC> = e.children.iter().map(el_to_zui).collect();
    let cn = children.len();
    let ch_ptr = if cn > 0 {
        let mut b = children.into_boxed_slice();
        let p = b.as_mut_ptr();
        std::mem::forget(b);
        p
    } else {
        std::ptr::null_mut()
    };
    // every stored handler becomes a Z_FUNC in the handlers list —
    // the renderer fires them all on click (click is the only event)
    let fns: Vec<ZValC> = e
        .handlers
        .iter()
        .map(|(_, f)| val_to_zvalc(&Value::Func(f.clone())))
        .collect();
    let hn = fns.len();
    let hv_ptr = if hn > 0 {
        let mut b = fns.into_boxed_slice();
        let p = b.as_mut_ptr();
        std::mem::forget(b);
        p
    } else {
        std::ptr::null_mut()
    };
    let handlers = ZValC {
        tag: if hn > 0 { Z_LIST } else { Z_NOTHING },
        _pad: 0,
        aux: 0,
        u: ZValU {
            vec: Box::into_raw(Box::new(ZVecC {
                v: hv_ptr,
                n: hn,
                cap: hn,
            })),
        },
    };
    Box::into_raw(Box::new(ZUiC {
        kind: cstr(&e.kind),
        props: props_ptr,
        pn,
        pcap: pn,
        ch: ch_ptr,
        cn,
        ccap: cn,
        handlers,
    }))
}

/* ---------- tree teardown (refresh frees the previous marshal) ---------- */

unsafe fn free_zvalc(z: *mut ZValC) {
    if z.is_null() {
        return;
    }
    let tag = unsafe { (*z).tag };
    unsafe {
        // union payload: bitwise copy out (all fields are POD pointers)
        let u = std::ptr::read(&(*z).u);
        match tag {
            Z_STR | 11 /* Z_TYPE */ => {
                if !u.s.is_null() {
                    drop(CString::from_raw(u.s));
                }
            }
            Z_LIST | 8 /* Z_TUPLE */ => {
                if !u.vec.is_null() {
                    let v = &mut *u.vec;
                    if !v.v.is_null() {
                        for i in 0..v.n {
                            free_zvalc(v.v.add(i));
                        }
                        drop(Vec::from_raw_parts(v.v, v.n, v.cap));
                    }
                    drop(Box::from_raw(u.vec));
                }
            }
            Z_FUNC => {
                if !u.fnp.is_null() {
                    drop(Box::from_raw(u.fnp as *mut HostFn));
                }
            }
            Z_BYTES => {
                if !u.by.p.is_null() {
                    drop(Vec::from_raw_parts(u.by.p, u.by.n, u.by.n));
                }
            }
            _ => {}
        }
    }
}

unsafe fn free_zui(u: *mut ZUiC) {
    if u.is_null() {
        return;
    }
    let u = unsafe { &mut *u };
    unsafe {
        if !u.kind.is_null() {
            drop(CString::from_raw(u.kind));
        }
        if !u.props.is_null() {
            for i in 0..u.pn {
                let p = &mut *u.props.add(i);
                free_zvalc(&mut p.k);
                free_zvalc(&mut p.v);
            }
            drop(Vec::from_raw_parts(u.props, u.pn, u.pcap));
        }
        if !u.ch.is_null() {
            for i in 0..u.cn {
                free_zui(*u.ch.add(i));
            }
            drop(Vec::from_raw_parts(u.ch, u.cn, u.ccap));
        }
        free_zvalc(&mut u.handlers);
        drop(Box::from_raw(u));
    }
}

fn subtree_dirty(el: &Rc<RefCell<UiElement>>) -> bool {
    let e = el.borrow();
    e.dirty || e.children.iter().any(subtree_dirty)
}

fn clear_subtree(el: &Rc<RefCell<UiElement>>) {
    let kids = {
        let mut e = el.borrow_mut();
        e.dirty = false;
        e.children.clone()
    };
    for c in kids {
        clear_subtree(&c);
    }
}

unsafe extern "C" fn host_refresh() -> *mut ZUiC {
    unsafe {
        // nothing changed → hand back the live tree, no re-marshal.
        // Pointer motion fires refresh constantly; this keeps it O(1).
        let el = HOST_EL.with(|c| c.borrow().clone());
        let Some(el) = el else {
            return LAST_TREE;
        };
        if !subtree_dirty(&el) {
            return LAST_TREE;
        }
        clear_subtree(&el);
        if !LAST_TREE.is_null() {
            free_zui(LAST_TREE);
        }
        let t = el_to_zui(&el);
        LAST_TREE = t;
        t
    }
}

unsafe extern "C" fn host_invoke(fv: *const ZValC) {
    if fv.is_null() {
        return;
    }
    let (tag, fnp) = unsafe { ((*fv).tag, (*fv).u.fnp) };
    if tag != Z_FUNC || fnp.is_null() {
        return;
    }
    let hf = unsafe { &*(fnp as *const HostFn) };
    let func = hf.f.clone();
    let vm = unsafe { &mut *HOST_VM };
    let _ = vm.call_gui_func(&func, vec![Value::Nothing]);
}

/// Run the native event loop for `win`. Blocks until the window closes.
/// `vm` is the live interpreter — passed as a raw pointer because the
/// renderer re-enters it from click handlers while `ui.run` is on the
/// stack (single-threaded; the outer borrow is suspended, not aliased).
pub fn run(win: &Rc<RefCell<UiElement>>, vm: *mut Vm, efficient: bool) -> i32 {
    unsafe {
        HOST_VM = vm;
        HOST_EL.with(|c| *c.borrow_mut() = Some(win.clone()));
        LAST_TREE = el_to_zui(win);
        let cb: ZInvokeCb = Some(host_invoke);
        let rcb: ZRefreshCb = Some(host_refresh);
        let r = zap_gui_run(LAST_TREE, cb, rcb, efficient as i32);
        free_zui(LAST_TREE);
        LAST_TREE = std::ptr::null_mut();
        HOST_EL.with(|c| *c.borrow_mut() = None);
        HOST_VM = std::ptr::null_mut();
        r
    }
}
