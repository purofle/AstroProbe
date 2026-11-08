//! Find the target methods through the exported il2cpp API, by name, so the
//! hook does not depend on hard-coded RVAs of one particular game build.
use std::{
    ffi::{c_char, c_int, c_void, CStr},
    ptr::null_mut,
    sync::atomic::{AtomicPtr, Ordering},
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
type FnThreadAttach = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnThreadDetach = unsafe extern "C" fn(*mut c_void);
type FnAsmOpen = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type FnAsmImage = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnClassFromName = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> *mut c_void;
type FnMethodFromName = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> *const *mut c_void;

struct Api {
    get_corlib: FnVoidPtr,
    domain_get: FnVoidPtr,
    thread_attach: FnThreadAttach,
    thread_detach: Option<FnThreadDetach>,
    assembly_open: FnAsmOpen,
    assembly_image: FnAsmImage,
    class_from_name: FnClassFromName,
    method_from_name: FnMethodFromName,
}

unsafe fn sym<T>(h: *mut c_void, name: &CStr) -> Result<T, String> {
    let p = libc::dlsym(h, name.as_ptr());
    if p.is_null() {
        return Err(format!("dlsym {name:?} failed"));
    }
    Ok(std::mem::transmute_copy(&p))
}

fn wait_for_lib(timeout: Duration) -> Result<*mut c_void, String> {
    let t0 = Instant::now();
    loop {
        let h = unsafe { libc::dlopen(LIB.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
        if !h.is_null() {
            log::info!("libil2cpp.so loaded after {:?}", t0.elapsed());
            return Ok(h);
        }
        if t0.elapsed() > timeout {
            return Err("timed out waiting for libil2cpp.so".into());
        }
        thread::sleep(Duration::from_millis(5));
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

pub fn wait_and_resolve() -> Result<Targets, String> {
    let h = wait_for_lib(Duration::from_secs(120))?;
    let api = unsafe {
        Api {
            get_corlib: sym(h, c"il2cpp_get_corlib")?,
            domain_get: sym(h, c"il2cpp_domain_get")?,
            thread_attach: sym(h, c"il2cpp_thread_attach")?,
            thread_detach: sym(h, c"il2cpp_thread_detach").ok(),
            assembly_open: sym(h, c"il2cpp_domain_assembly_open")?,
            assembly_image: sym(h, c"il2cpp_assembly_get_image")?,
            class_from_name: sym(h, c"il2cpp_class_from_name")?,
            method_from_name: sym(h, c"il2cpp_class_get_method_from_name")?,
        }
    };

    // il2cpp_get_corlib() just returns il2cpp_defaults.corlib: NULL until
    // Runtime::Init has loaded mscorlib, safe to poll.
    let t0 = Instant::now();
    while unsafe { (api.get_corlib)() }.is_null() {
        if t0.elapsed() > Duration::from_secs(120) {
            return Err("timed out waiting for il2cpp_init".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    log::info!("il2cpp corlib loaded after {:?}", t0.elapsed());

    // corlib is set *before* the domain exists. il2cpp_domain_get() creates
    // the domain on demand with a GC allocation, which aborts on a thread the
    // GC doesn't know about (us). So never call it until the domain exists:
    // read the static `S_domain` that Domain::GetCurrent checks, and wait for
    // Runtime::Init to finish filling it in.
    let domain = match unsafe { find_domain_slot(api.domain_get as *const u32) } {
        Some(slot) => {
            log::info!("S_domain slot @ {:p} (rva {:#x})", slot, rva(slot as *const c_void));
            wait_domain(slot, Duration::from_secs(120))?
        }
        None => {
            // unknown codegen: fall back to a generous delay after corlib
            log::warn!("could not locate S_domain, falling back to a fixed delay");
            thread::sleep(Duration::from_secs(5));
            null_mut()
        }
    };
    log::info!("il2cpp domain ready after {:?}", t0.elapsed());

    unsafe {
        let domain = if domain.is_null() { (api.domain_get)() } else { domain };
        if domain.is_null() {
            return Err("il2cpp_domain_get returned NULL".into());
        }
        let thread = (api.thread_attach)(domain);

        // assemblies may still be registering right after init: retry briefly
        let mut result = Err(String::new());
        for _ in 0..50 {
            result = resolve(&api, domain);
            if result.is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        if let (Some(detach), false) = (api.thread_detach, thread.is_null()) {
            detach(thread);
        }
        result
    }
}

/// Il2CppDomain.friendly_name — written by Runtime::Init right after the domain
/// is allocated, together with domain/setup/domain_id.
const DOMAIN_FRIENDLY_NAME: usize = 0x20;

fn wait_domain(slot: *mut *mut c_void, timeout: Duration) -> Result<*mut c_void, String> {
    let slot = unsafe { AtomicPtr::from_ptr(slot) };
    let t0 = Instant::now();
    loop {
        let d = slot.load(Ordering::Acquire);
        if !d.is_null() {
            let name = unsafe { AtomicPtr::from_ptr(d.byte_add(DOMAIN_FRIENDLY_NAME) as *mut *mut c_void) };
            if !name.load(Ordering::Acquire).is_null() {
                // let Runtime::Init finish the rest (main thread attach, contexts...)
                thread::sleep(Duration::from_millis(200));
                return Ok(d);
            }
        }
        if t0.elapsed() > timeout {
            return Err("timed out waiting for the il2cpp domain".into());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// il2cpp_domain_get is `B Domain::GetCurrent`, which starts with
///   adrp xN, page(S_domain) ; ldr x0, [xN, #off] ; cbnz x0, ...
/// Decode that to get &S_domain without running the function.
unsafe fn find_domain_slot(export: *const u32) -> Option<*mut *mut c_void> {
    let mut pc = export;
    let insn = pc.read();
    if insn & 0xFC00_0000 == 0x1400_0000 {
        // B imm26
        let off = (((insn & 0x03FF_FFFF) << 6) as i32 >> 6) as isize;
        pc = pc.offset(off);
    }
    for i in 0..8 {
        let a = pc.add(i);
        let adrp = a.read();
        if adrp & 0x9F00_0000 != 0x9000_0000 {
            continue;
        }
        let rd = adrp & 0x1F;
        let immlo = ((adrp >> 29) & 0x3) as i64;
        let immhi = ((adrp >> 5) & 0x7FFFF) as i64;
        let imm = (((immhi << 2) | immlo) << 44) >> 32; // sign-extend 21 bits, << 12
        let page = ((a as i64) & !0xFFF) + imm;
        // the matching LDR x0, [xN, #imm12*8] within the next few instructions
        for j in 1..4 {
            let ldr = a.add(j).read();
            if ldr & 0xFFC0_0000 == 0xF940_0000 && (ldr >> 5) & 0x1F == rd && ldr & 0x1F == 0 {
                let off = (((ldr >> 10) & 0xFFF) * 8) as i64;
                let cbnz = a.add(j + 1).read();
                // cbnz x0 — confirms this is the "already created?" check
                if cbnz & 0xFF00_001F != 0xB500_0000 {
                    return None;
                }
                return Some((page + off) as *mut *mut c_void);
            }
        }
        return None;
    }
    None
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

unsafe fn resolve(api: &Api, domain: *mut c_void) -> Result<Targets, String> {
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
