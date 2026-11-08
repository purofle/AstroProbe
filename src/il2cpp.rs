//! Find the target methods through the exported il2cpp API, by name, so the
//! hook does not depend on hard-coded RVAs of one particular game build.
//!
//! Timing follows ByNameModding (BNM): hook `il2cpp_init` as soon as
//! libil2cpp.so is loaded, let the original run, then resolve and hook the
//! game methods from inside the detour. At that point the runtime is fully
//! initialised and we are on the thread il2cpp_init attached as its main
//! thread, so the il2cpp API (and its GC allocations) is safe to use without
//! polling, thread_attach, or peeking at runtime internals.
use crate::{hooks, inline, is_lib, ws};
use std::{
    ffi::{c_char, c_int, c_void, CStr},
    panic::{catch_unwind, AssertUnwindSafe},
    ptr::null_mut,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    thread,
    time::{Duration, Instant},
};

const LIB: &CStr = c"libil2cpp.so";
const ASSEMBLY: &CStr = c"AstroDX.Runtime";

/// (namespace, class, method, arg count)
pub const PUSH: (&CStr, &CStr, &CStr, c_int) =
    (c"AstroDX.Models.Scoring.Metrics", c"Statistics", c"Push", 2);
pub const TALLY_NOTE: (&CStr, &CStr, &CStr, c_int) =
    (c"AstroDX.Gameplay.Systems", c"StatisticsSystem", c"TallyNote", 3);

pub struct Targets {
    /// Statistics.Push(in NoteCategory, in JudgeData) — every judgement goes through it
    pub push: *mut c_void,
    /// StatisticsSystem.TallyNote(Note, JudgeData, double) — supplies note context
    pub tally_note: Option<*mut c_void>,
}

type FnVoidPtr = unsafe extern "C" fn() -> *mut c_void;
type FnInit = unsafe extern "C" fn(*const c_char) -> c_int;
type FnAsmOpen = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type FnAsmImage = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnClassFromName = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> *mut c_void;
type FnMethodFromName = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> *const *mut c_void;

struct Api {
    domain_get: FnVoidPtr,
    assembly_open: FnAsmOpen,
    assembly_image: FnAsmImage,
    class_from_name: FnClassFromName,
    method_from_name: FnMethodFromName,
}

impl Api {
    unsafe fn load(h: *mut c_void) -> Result<Self, String> {
        Ok(Self {
            domain_get: sym(h, c"il2cpp_domain_get")?,
            assembly_open: sym(h, c"il2cpp_domain_assembly_open")?,
            assembly_image: sym(h, c"il2cpp_assembly_get_image")?,
            class_from_name: sym(h, c"il2cpp_class_from_name")?,
            method_from_name: sym(h, c"il2cpp_class_get_method_from_name")?,
        })
    }
}

/// libil2cpp.so handle, set once il2cpp_init is hooked
static HANDLE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static ORIG_INIT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// claimed by whichever path (GOT dlopen hook or fallback poll) hooks il2cpp_init first
static INIT_CLAIMED: AtomicBool = AtomicBool::new(false);
/// the il2cpp_init detour has run its post-init work
static READY: AtomicBool = AtomicBool::new(false);

unsafe fn sym<T>(h: *mut c_void, name: &CStr) -> Result<T, String> {
    let p = libc::dlsym(h, name.as_ptr());
    if p.is_null() {
        return Err(format!("dlsym {name:?} failed"));
    }
    Ok(std::mem::transmute_copy(&p))
}

/// Called by the libmain dlopen detour (got.rs) after every successful dlopen,
/// on the Java thread running NativeLoader.load().
pub fn on_dlopen(name: &CStr, handle: *mut c_void) {
    if !is_lib(name.to_bytes(), LIB.to_bytes()) {
        return;
    }
    log::info!("libil2cpp.so loaded by libmain ({:?} after load)", crate::since_load());
    if let Err(e) = hook_init(handle) {
        log::error!("{e}");
    }
}

/// Fallback when the GOT hook could not be installed: poll for the library like
/// before. Racy in theory (il2cpp_init could run before we notice the library),
/// in practice libunity calls it long after NativeLoader.load() returns.
pub fn poll_for_lib(timeout: Duration) {
    let t0 = Instant::now();
    loop {
        if INIT_CLAIMED.load(Ordering::Acquire) {
            return;
        }
        let h = unsafe { libc::dlopen(LIB.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
        if !h.is_null() {
            log::info!("libil2cpp.so found by polling after {:?}", t0.elapsed());
            if let Err(e) = hook_init(h) {
                log::error!("{e}");
            }
            return;
        }
        if t0.elapsed() > timeout {
            log::error!("timed out waiting for libil2cpp.so");
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn hook_init(handle: *mut c_void) -> Result<(), String> {
    if INIT_CLAIMED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    unsafe {
        let init: *mut c_void = sym(handle, c"il2cpp_init")?;
        // il2cpp_get_corlib() just returns il2cpp_defaults.corlib: NULL until
        // Runtime::Init has loaded mscorlib. Non-NULL means we are too late.
        let corlib: FnVoidPtr = sym(handle, c"il2cpp_get_corlib")?;
        if !corlib().is_null() {
            return Err("il2cpp_init already ran before it could be hooked; no judgements this session".into());
        }
        HANDLE.store(handle, Ordering::Release);
        log::info!("il2cpp_init @ {init:p} (rva {:#x})", rva(init));
        inline::hook("il2cpp_init", init, il2cpp_init_detour as *mut c_void, &ORIG_INIT)
    }
}

unsafe extern "C" fn il2cpp_init_detour(domain_name: *const c_char) -> c_int {
    let orig: FnInit = std::mem::transmute(ORIG_INIT.load(Ordering::Acquire));
    let ret = orig(domain_name);
    if !READY.swap(true, Ordering::AcqRel) {
        // never let our code take the game down
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| on_runtime_ready(domain_name, ret))) {
            log::error!("post-init panicked: {:?}", p.downcast_ref::<&str>());
        }
    }
    ret
}

fn on_runtime_ready(domain_name: *const c_char, ret: c_int) {
    let name = (!domain_name.is_null()).then(|| unsafe { CStr::from_ptr(domain_name) });
    log::info!("il2cpp_init({name:?}) = {ret} ({:?} after load), resolving targets", crate::since_load());
    let result = unsafe { Api::load(HANDLE.load(Ordering::Acquire)).and_then(|api| resolve(&api)) }
        .and_then(|targets| hooks::install(&targets));
    match result {
        Ok(()) => {
            ws::set_hooked(true);
            log::info!("hooks installed, judgements will be streamed");
        }
        Err(e) => log::error!("hook install failed: {e}"),
    }
}

/// Image base of libil2cpp.so, only used to log RVAs for cross-checking with IDA.
fn rva(p: *const c_void) -> usize {
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(p, &mut info) != 0 {
            return p as usize - info.dli_fbase as usize;
        }
    }
    0
}

unsafe fn method(api: &Api, image: *mut c_void, spec: (&CStr, &CStr, &CStr, c_int)) -> Result<*mut c_void, String> {
    let (ns, cls, name, argc) = spec;
    let klass = (api.class_from_name)(image, ns.as_ptr(), cls.as_ptr());
    if klass.is_null() {
        return Err(format!("class {ns:?}.{cls:?} not found"));
    }
    let mi = (api.method_from_name)(klass, name.as_ptr(), argc);
    if mi.is_null() {
        return Err(format!("method {cls:?}::{name:?}/{argc} not found"));
    }
    // MethodInfo.methodPointer is the first field
    let ptr = *mi;
    if ptr.is_null() {
        return Err(format!("{cls:?}::{name:?} has no code (stripped/generic?)"));
    }
    log::info!("resolved {}.{}::{} @ {:p} (rva {:#x})",
        ns.to_string_lossy(), cls.to_string_lossy(), name.to_string_lossy(), ptr, rva(ptr));
    Ok(ptr)
}

/// Must run on a thread attached to il2cpp, after il2cpp_init returned.
unsafe fn resolve(api: &Api) -> Result<Targets, String> {
    let domain = (api.domain_get)();
    if domain.is_null() {
        return Err("il2cpp_domain_get returned NULL".into());
    }
    let asm = (api.assembly_open)(domain, ASSEMBLY.as_ptr());
    if asm.is_null() {
        return Err(format!("assembly {ASSEMBLY:?} not found"));
    }
    let image = (api.assembly_image)(asm);
    if image.is_null() {
        return Err("assembly image is NULL".into());
    }
    let push = method(api, image, PUSH)?;
    let tally_note = match method(api, image, TALLY_NOTE) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("{e}; judgements will be sent without note location");
            None
        }
    };
    Ok(Targets { push, tally_note })
}
