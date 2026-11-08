//! Inline hooks via ShadowHook (bytedance/android-inline-hook, MIT), compiled
//! into this library by build.rs.
//!
//! `hook()` publishes the original-function trampoline into `orig` *before*
//! the target is patched, so a detour that runs on another thread the instant
//! the patch lands never sees a null original.
use std::{
    ffi::{c_char, c_int, c_void, CStr},
    sync::{
        atomic::{AtomicPtr, Ordering},
        OnceLock,
    },
};

const SHADOWHOOK_MODE_UNIQUE: c_int = 1;

extern "C" {
    fn shadowhook_init(default_mode: c_int, debuggable: bool) -> c_int;
    fn shadowhook_get_version() -> *const c_char;
    fn shadowhook_get_errno() -> c_int;
    fn shadowhook_to_errmsg(error_number: c_int) -> *const c_char;
    fn shadowhook_hook_func_addr(func_addr: *mut c_void, new_addr: *mut c_void, orig_addr: *mut *mut c_void) -> *mut c_void;
}

fn errmsg(e: c_int) -> String {
    let p = unsafe { shadowhook_to_errmsg(e) };
    let msg = if p.is_null() { "?".into() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy() };
    format!("errno {e}: {msg}")
}

/// shadowhook_init hooks the linker's soinfo::call_constructors and dlopens
/// libshadowhook_nothing.so (shipped in the APK next to libjudgehook.so).
/// Never call this from inside .init_array: it runs lazily on the first hook.
fn init() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        // unique mode: one proxy per address, orig is called directly (no CALL_PREV/POP_STACK)
        let r = unsafe { shadowhook_init(SHADOWHOOK_MODE_UNIQUE, false) };
        let ver = unsafe { CStr::from_ptr(shadowhook_get_version()) }.to_string_lossy();
        if r == 0 {
            log::info!("shadowhook {ver} initialised (unique mode)");
            Ok(())
        } else {
            Err(format!("shadowhook {ver} init failed: {}", errmsg(r)))
        }
    })
    .clone()
}

pub unsafe fn hook(name: &str, target: *mut c_void, detour: *mut c_void, orig: &'static AtomicPtr<c_void>) -> Result<(), String> {
    init()?;
    // orig_addr must stay valid for the life of the hook: point it at the static
    let stub = shadowhook_hook_func_addr(target, detour, orig.as_ptr());
    if stub.is_null() {
        return Err(format!("{name}: shadowhook failed, {}", errmsg(shadowhook_get_errno())));
    }
    log::info!("hooked {name} @ {target:p}, orig {:p}", orig.load(Ordering::Acquire));
    Ok(())
}
