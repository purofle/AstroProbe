//! Detours on the il2cpp-compiled game methods (backend: see inline.rs).
use crate::events::{self, Judge, JudgeData, NoteCtx, SongInfo};
use crate::il2cpp::{SongTargets, Targets};
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
        events::emit(Judge::new(*category, *data, ctx));
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

// ---------------------------------------------------------------- song lifecycle

// Field offsets (Il2CppDumper dump.cs)
const DATA_SYSTEM_DIFFICULTY: usize = 0x28; // DataSystem.<Difficulty>k__BackingField
const DIFF_ID: usize = 0x10; // Difficulty.<ID>k__BackingField (int, &inote_<id>)
const DIFF_CHARTER: usize = 0x20; // Difficulty.<Charter>k__BackingField
const DIFF_VALUE: usize = 0x78; // Difficulty.<Value>k__BackingField (level text)
const DIFF_PARENT_LEVEL: usize = 0xA8; // Difficulty.<ParentLevel>k__BackingField
const LEVEL_TITLE: usize = 0x20;
const LEVEL_ARTIST: usize = 0x28;
// System.String: Il2CppObject header, int32 length, UTF-16 chars
const STRING_LENGTH: usize = 0x10;
const STRING_CHARS: usize = 0x14;

type InstanceFn = unsafe extern "C" fn(this: *mut c_void, method: *const c_void);
type StaticFn = unsafe extern "C" fn(method: *const c_void);
type ObjectGetterFn = unsafe extern "C" fn(this: *mut c_void, method: *const c_void) -> *mut u8;

static ORIG_DATA_INIT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ORIG_FINISH: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ORIG_RESTART: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ORIG_QUIT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ALIAS_FN: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ALIAS_MI: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

unsafe fn obj_at(obj: *const u8, off: usize) -> *const u8 {
    ptr::read_unaligned(obj.add(off) as *const *const u8)
}

unsafe fn string(s: *const u8) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let len = ptr::read_unaligned(s.add(STRING_LENGTH) as *const i32);
    if !(0..=4096).contains(&len) {
        return None;
    }
    let chars = std::slice::from_raw_parts(s.add(STRING_CHARS) as *const u16, len as usize);
    Some(String::from_utf16_lossy(chars))
}

unsafe fn song_info(data_system: *const u8) -> SongInfo {
    let mut info = SongInfo::default();
    let diff = obj_at(data_system, DATA_SYSTEM_DIFFICULTY);
    if diff.is_null() {
        return info;
    }
    info.difficulty_id = ptr::read_unaligned(diff.add(DIFF_ID) as *const i32);
    info.level = string(obj_at(diff, DIFF_VALUE));
    info.charter = string(obj_at(diff, DIFF_CHARTER)).filter(|c| !c.is_empty());
    let level = obj_at(diff, DIFF_PARENT_LEVEL);
    if !level.is_null() {
        info.title = string(obj_at(level, LEVEL_TITLE));
        info.artist = string(obj_at(level, LEVEL_ARTIST));
    }
    let alias = ALIAS_FN.load(Ordering::Acquire);
    if !alias.is_null() {
        let get: ObjectGetterFn = std::mem::transmute(alias);
        info.difficulty = string(get(diff as *mut c_void, ALIAS_MI.load(Ordering::Acquire)));
    }
    info
}

unsafe extern "C" fn data_init_detour(this: *mut c_void, method: *const c_void) {
    let orig: InstanceFn = std::mem::transmute(ORIG_DATA_INIT.load(Ordering::Acquire));
    orig(this, method);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let info = if this.is_null() { SongInfo::default() } else { song_info(this as *const u8) };
        log::info!("song start: {info:?}");
        events::song_start(info);
    }));
}

unsafe extern "C" fn finish_detour(this: *mut c_void, method: *const c_void) {
    let _ = catch_unwind(|| events::song_end("finish"));
    let orig: InstanceFn = std::mem::transmute(ORIG_FINISH.load(Ordering::Acquire));
    orig(this, method);
}

unsafe extern "C" fn restart_detour(method: *const c_void) {
    let _ = catch_unwind(|| events::song_end("restart"));
    let orig: StaticFn = std::mem::transmute(ORIG_RESTART.load(Ordering::Acquire));
    orig(method);
}

unsafe extern "C" fn quit_detour(method: *const c_void) {
    let _ = catch_unwind(|| events::song_end("quit"));
    let orig: StaticFn = std::mem::transmute(ORIG_QUIT.load(Ordering::Acquire));
    orig(method);
}

unsafe fn install_song_hooks(t: &SongTargets) {
    if let Some((code, mi)) = t.alias {
        ALIAS_MI.store(mi as *mut c_void, Ordering::Release);
        ALIAS_FN.store(code, Ordering::Release);
    }
    let hooks: [(&str, Option<*mut c_void>, *mut c_void, &'static AtomicPtr<c_void>); 4] = [
        ("DataSystem.Initialize", t.data_init, data_init_detour as *mut c_void, &ORIG_DATA_INIT),
        ("GameSystem.Finish", t.finish, finish_detour as *mut c_void, &ORIG_FINISH),
        ("GameSystem.Restart", t.restart, restart_detour as *mut c_void, &ORIG_RESTART),
        ("GameSystem.Quit", t.quit, quit_detour as *mut c_void, &ORIG_QUIT),
    ];
    for (name, target, detour, orig) in hooks {
        if let Some(target) = target {
            if let Err(e) = inline::hook(name, target, detour, orig) {
                log::warn!("{e}; no song event from it");
            }
        }
    }
}

pub fn install(t: &Targets) -> Result<(), String> {
    unsafe {
        install_song_hooks(&t.song);
        // Install the context hook first: Push is called from inside TallyNote.
        if let Some(tn) = t.tally_note {
            if let Err(e) = inline::hook("StatisticsSystem.TallyNote", tn, tally_note_detour as *mut c_void, &ORIG_TALLY_NOTE) {
                log::warn!("{e}; continuing without note context");
            }
        }
        inline::hook("Statistics.Push", t.push, push_detour as *mut c_void, &ORIG_PUSH)
    }
}
