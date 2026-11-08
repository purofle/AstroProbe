//! Detours on the il2cpp-compiled game methods (backend: see inline.rs).
use crate::events::{self, Event, JudgeData, NoteCtx};
use crate::il2cpp::Targets;
use crate::inline;
use std::{
    cell::Cell,
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

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

pub fn install(t: &Targets) -> Result<(), String> {
    unsafe {
        // Install the context hook first: Push is called from inside TallyNote.
        if let Some(tn) = t.tally_note {
            if let Err(e) = inline::hook("StatisticsSystem.TallyNote", tn, tally_note_detour as *mut c_void, &ORIG_TALLY_NOTE) {
                log::warn!("{e}; continuing without note context");
            }
        }
        inline::hook("Statistics.Push", t.push, push_detour as *mut c_void, &ORIG_PUSH)
    }
}
