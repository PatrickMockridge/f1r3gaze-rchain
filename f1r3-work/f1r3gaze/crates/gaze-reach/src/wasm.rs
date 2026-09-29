//! The module's exports and its two host imports. The only unsafe code in
//! the reach tier: copies to and from buffers whose lengths the caller gave.
#![allow(unsafe_code)]

use crate::json::J;
use crate::{Host, ReachTab, accept_script, compile_inline};
use std::cell::RefCell;
use std::collections::BTreeMap;

#[link(wasm_import_module = "gaze")]
unsafe extern "C" {
    /// Run a host DOM command (JSON at `ptr`); returns the answer's length.
    fn host_call(ptr: *const u8, len: usize) -> usize;
    /// Copy the last answer to `out`.
    fn host_result(out: *mut u8);
}

pub struct JsHost;

impl Host for JsHost {
    fn call(&self, cmd: &J) -> J {
        let s = cmd.to_string();
        let n = unsafe { host_call(s.as_ptr(), s.len()) };
        let mut buf = vec![0u8; n];
        if n > 0 {
            unsafe { host_result(buf.as_mut_ptr()) };
        }
        std::str::from_utf8(&buf).ok().and_then(|t| J::parse(t).ok()).unwrap_or(J::Null)
    }
}

#[derive(Default)]
struct Registry {
    tabs: BTreeMap<u32, ReachTab<JsHost>>,
    next: u32,
    result: Vec<u8>,
}

thread_local! {
    static REG: RefCell<Registry> = RefCell::new(Registry::default());
}

unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if n == 0 { &[] } else { unsafe { std::slice::from_raw_parts(p, n) } }
}
unsafe fn text<'a>(p: *const u8, n: usize) -> &'a str {
    std::str::from_utf8(unsafe { bytes(p, n) }).unwrap_or("")
}

fn set_result(s: String) -> usize {
    REG.with(|r| {
        let mut r = r.borrow_mut();
        r.result = s.into_bytes();
        r.result.len()
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gaze_alloc(len: usize) -> *mut u8 {
    let mut v = vec![0u8; len.max(1)];
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_free(p: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(p, len.max(1), len.max(1)) });
}

#[unsafe(no_mangle)]
pub extern "C" fn gaze_result_len() -> usize {
    REG.with(|r| r.borrow().result.len())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_result_read(out: *mut u8) {
    REG.with(|r| {
        let r = r.borrow();
        unsafe { std::ptr::copy_nonoverlapping(r.result.as_ptr(), out, r.result.len()) };
    })
}

fn install(k: Result<gaze_knf::Knf, String>, seed: [u8; 32]) -> u32 {
    match k.and_then(|k| ReachTab::load(&k, JsHost, seed)) {
        Ok(t) => REG.with(|r| {
            let mut r = r.borrow_mut();
            r.next += 1;
            let h = r.next;
            r.tabs.insert(h, t);
            h
        }),
        Err(e) => {
            set_result(e);
            0
        }
    }
}

/// Load a `.knf` fetched by the host, checked against `integrity`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_load_knf(p: *const u8, n: usize, ip: *const u8, il: usize, seed: *const u8) -> u32 {
    let k = accept_script(unsafe { bytes(p, n) }, Some(unsafe { text(ip, il) }));
    let mut s = [0u8; 32];
    s.copy_from_slice(unsafe { bytes(seed, 32) });
    install(k, s)
}

/// Load inline kernel text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_load_text(
    p: *const u8,
    n: usize,
    ip: *const u8,
    il: usize,
    lp: *const u8,
    ll: usize,
    seed: *const u8,
) -> u32 {
    let k = compile_inline(unsafe { text(p, n) }, unsafe { text(ip, il) }, unsafe { text(lp, ll) });
    let mut s = [0u8; 32];
    s.copy_from_slice(unsafe { bytes(seed, 32) });
    install(k, s)
}

fn add(h: u32, k: Result<gaze_knf::Knf, String>) -> i32 {
    let r = k.and_then(|k| REG.with(|r| r.borrow_mut().tabs.get_mut(&h).ok_or("no tab".to_string())?.add_script(&k)));
    match r {
        Ok(()) => 1,
        Err(e) => {
            set_result(e);
            0
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_add_knf(h: u32, p: *const u8, n: usize, ip: *const u8, il: usize) -> i32 {
    add(h, accept_script(unsafe { bytes(p, n) }, Some(unsafe { text(ip, il) })))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_add_text(h: u32, p: *const u8, n: usize, ip: *const u8, il: usize, lp: *const u8, ll: usize) -> i32 {
    add(h, compile_inline(unsafe { text(p, n) }, unsafe { text(ip, il) }, unsafe { text(lp, ll) }))
}

/// Run a frame; the result buffer holds the frame's JSON.
#[unsafe(no_mangle)]
pub extern "C" fn gaze_frame(h: u32, now_ms: f64) -> usize {
    // The tab is taken out of the registry while it runs: its DOM calls go
    // back to the host, which must not re-enter the registry.
    let tab = REG.with(|r| r.borrow_mut().tabs.remove(&h));
    let Some(mut tab) = tab else { return 0 };
    let out = tab.frame(now_ms.max(0.0) as u64).to_string();
    REG.with(|r| r.borrow_mut().tabs.insert(h, tab));
    set_result(out)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_dispatch(h: u32, node: u32, tp: *const u8, tl: usize, fp: *const u8, fl: usize, bubbles: u32) -> u32 {
    let ty = unsafe { text(tp, tl) }.to_string();
    let fields = J::parse(unsafe { text(fp, fl) }).unwrap_or(J::Null);
    let tab = REG.with(|r| r.borrow_mut().tabs.remove(&h));
    let Some(mut tab) = tab else { return 0 };
    let f = tab.dispatch(node, &ty, &fields, bubbles != 0);
    REG.with(|r| r.borrow_mut().tabs.insert(h, tab));
    f
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_net_done(
    h: u32,
    id: u32,
    status: i32,
    hp: *const u8,
    hl: usize,
    bp: *const u8,
    bl: usize,
    ep: *const u8,
    el: usize,
) {
    let headers = J::parse(unsafe { text(hp, hl) }).unwrap_or(J::Null);
    let body = unsafe { bytes(bp, bl) }.to_vec();
    let failure = unsafe { text(ep, el) }.to_string();
    REG.with(|r| {
        if let Some(t) = r.borrow_mut().tabs.get_mut(&h) {
            t.net_done(id, status as i64, &headers, &body, (!failure.is_empty()).then_some(failure.as_str()));
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gaze_store_done(h: u32, id: u32, p: *const u8, n: usize) {
    let j = J::parse(unsafe { text(p, n) }).unwrap_or(J::Null);
    REG.with(|r| {
        if let Some(t) = r.borrow_mut().tabs.get_mut(&h) {
            t.store_done(id, &j);
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gaze_close(h: u32) {
    REG.with(|r| r.borrow_mut().tabs.remove(&h));
}
