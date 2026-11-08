//! Inline hooks (And64InlineHook) on the il2cpp-compiled methods.
use crate::events::{self, Event, JudgeData, NoteCtx};
use crate::il2cpp::Targets;
use std::{
    cell::Cell,
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

extern "C" {
    fn A64HookFunctionV(symbol: *mut c_void, replace: *mut c_void, rwx: *mut c_void, rwx_size: usize) -> *mut c_void;
    fn A64HookFunction(symbol: *mut c_void, replace: *mut c_void, result: *mut *mut c_void);
}

/// A64HookFunctionV counts rwx_size in instructions: A64_MAX_INSTRUCTIONS(5) * 10
const TRAMPOLINE_INSNS: usize = 50;
const TRAMPOLINE_BYTES: usize = 256;

// Offsets in SimaiSharp.Structures.Note (from Il2CppDumper dump.cs)
const NOTE_LENGTH: usize = 0x10;
const NOTE_CATEGORY: usize = 0x14;
const NOTE_STYLES: usize = 0x18;
const NOTE_LOCATION: usize = 0x1C;

type PushFn = unsafe extern "C" fn(this: *mut c_void, category: *const i32, data: *const JudgeData, method: *const c_void);
type TallyNoteFn = unsafe extern "C" fn(this: *mut c_void, note: *mut u8, data: JudgeData, timing: f64, method: *const c_void);

static ORIG_PUSH: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ORIG_TALLY_NOTE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

thread_local! {
    /// set by TallyNote around the call to the original, read by Push
    static CURRENT_NOTE: Cell<Option<NoteCtx>> = const { Cell::new(None) };
}

unsafe extern "C" fn push_detour(this: *mut c_void, category: *const i32, data: *const JudgeData, method: *const c_void) {
    let orig: PushFn = std::mem::transmute(ORIG_PUSH.load(Ordering::Acquire));
    orig(this, category, data, method);

    // observe only; never let our code take the game down
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if category.is_null() || data.is_null() {
            return;
        }
        let ctx = CURRENT_NOTE.with(|c| c.get());
        events::emit(Event::new(*category, *data, ctx));
    }));
}

unsafe extern "C" fn tally_note_detour(this: *mut c_void, note: *mut u8, data: JudgeData, timing: f64, method: *const c_void) {
    let ctx = (!note.is_null()).then(|| NoteCtx {
        length: ptr::read_unaligned(note.add(NOTE_LENGTH) as *const f32),
        category: ptr::read_unaligned(note.add(NOTE_CATEGORY) as *const i32),
        styles: ptr::read_unaligned(note.add(NOTE_STYLES) as *const i32),
        location: ptr::read_unaligned(note.add(NOTE_LOCATION) as *const i32),
        timing,
    });
    let prev = CURRENT_NOTE.with(|c| c.replace(ctx));
    let orig: TallyNoteFn = std::mem::transmute(ORIG_TALLY_NOTE.load(Ordering::Acquire));
    orig(this, note, data, timing, method);
    CURRENT_NOTE.with(|c| c.set(prev));
}

/// One anonymous RWX page holding the relocated prologues (trampolines).
struct TrampolinePage {
    base: *mut u8,
    used: usize,
    size: usize,
}

impl TrampolinePage {
    fn new() -> Option<Self> {
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let p = unsafe {
            libc::mmap(ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        if p == libc::MAP_FAILED {
            log::warn!("mmap RWX failed (errno {}), falling back to static trampoline pool",
                std::io::Error::last_os_error());
            return None;
        }
        Some(Self { base: p as *mut u8, used: 0, size })
    }
    fn alloc(&mut self) -> Option<*mut c_void> {
        (self.used + TRAMPOLINE_BYTES <= self.size).then(|| {
            let p = unsafe { self.base.add(self.used) };
            self.used += TRAMPOLINE_BYTES;
            p as *mut c_void
        })
    }
}

unsafe fn hook(page: &mut Option<TrampolinePage>, name: &str, target: *mut c_void, detour: *mut c_void, orig: &AtomicPtr<c_void>) -> Result<(), String> {
    let trampoline = match page.as_mut().and_then(|p| p.alloc()) {
        Some(rwx) => A64HookFunctionV(target, detour, rwx, TRAMPOLINE_INSNS),
        None => {
            let mut t = ptr::null_mut();
            A64HookFunction(target, detour, &mut t);
            t
        }
    };
    if trampoline.is_null() {
        return Err(format!("{name}: A64Hook failed (see A64_HOOK in logcat)"));
    }
    orig.store(trampoline, Ordering::Release);
    log::info!("hooked {name} @ {target:p}, trampoline {trampoline:p}");
    Ok(())
}

pub fn install(t: &Targets) -> Result<(), String> {
    let mut page = TrampolinePage::new();
    unsafe {
        // Install the context hook first: Push is called from inside TallyNote.
        if let Some(tn) = t.tally_note {
            if let Err(e) = hook(&mut page, "StatisticsSystem.TallyNote", tn, tally_note_detour as *mut c_void, &ORIG_TALLY_NOTE) {
                log::warn!("{e}; continuing without note context");
            }
        }
        hook(&mut page, "Statistics.Push", t.push, push_detour as *mut c_void, &ORIG_PUSH)
    }
}
