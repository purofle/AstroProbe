//! Judgement event model + the non-blocking hand-off from the game thread.
use serde::Serialize;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
    OnceLock,
};

/// AstroDX.Models.Scoring.JudgeData (struct, 2 doubles; passed by value as an HFA)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct JudgeData {
    /// grade index + progress inside that window: 0.x Critical, 1.x Perfect,
    /// 2.x Great, 3.x Good, >=4 Miss (JudgementWindows.Interpolate)
    pub interpolation: f64,
    /// seconds, music time - note time (negative = early)
    pub offset: f64,
}

/// Note context captured in the TallyNote hook (SimaiSharp.Structures.Note fields)
#[derive(Clone, Copy, Debug)]
pub struct NoteCtx {
    pub category: i32,
    pub styles: i32,
    pub location: i32,
    pub length: f32,
    pub timing: f64,
}

const CATEGORY: [&str; 5] = ["Tap", "Touch", "Hold", "Slide", "Break"];
const GRADE: [&str; 5] = ["CriticalPerfect", "Perfect", "Great", "Good", "Miss"];

// SimaiSharp NoteStyles flags
const STYLE_EX: i32 = 8;
const STYLE_FIREWORKS: i32 = 16;
const STYLE_MINE: i32 = 32;

#[derive(Serialize)]
pub struct Event {
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u64,
    /// ms since the hook library was loaded (monotonic)
    t_ms: f64,
    cat: &'static str,
    grade: &'static str,
    grade_i: Option<i32>,
    /// Exact / Early / Late / Miss; null when the judgement carries no timing
    state: Option<&'static str>,
    /// null when `synthetic`
    offset_ms: Option<f64>,
    /// the game pushed one of the JudgeData constants (offset fixed at 0):
    /// misses, slides not finished in time, ... — there is no real offset
    synthetic: bool,
    interp: f64,
    /// "1".."8" for buttons, "A1".."E8"/"C1" for touch sensors, null for slides
    loc: Option<String>,
    ex: bool,
    fireworks: bool,
    mine: bool,
    note_cat: Option<&'static str>,
    hold_len: Option<f32>,
    /// `timing` argument of StatisticsSystem.TallyNote, in ms (null for slides):
    /// the offset the game hands to Effects.SpawnText for the FAST/LATE text.
    /// Equal to the offset for normal hits, the head offset for holds, 0 for misses.
    display_offset_ms: Option<f64>,
}

fn cat_name(c: i32) -> &'static str {
    usize::try_from(c).ok().and_then(|i| CATEGORY.get(i)).copied().unwrap_or("Unknown")
}

/// SimaiSharp location: low nibble = index (0-7), high nibble = group
/// (0 = outer button, 0xA..0xE = touch sensor letter).
fn loc_name(loc: i32) -> String {
    let idx = loc & 0xF;
    match (loc >> 4) & 0xF {
        0 => format!("{}", idx + 1),
        g @ 0xA..=0xF => format!("{:X}{}", g, idx + 1),
        g => format!("?{g}:{idx}"),
    }
}

/// JudgeData.CriticalPerfect/Perfect/Great/Good/Miss are (0..=4, 0.0). A real
/// measured hit never lands on an offset of exactly 0.0 seconds.
fn is_constant(d: &JudgeData) -> bool {
    d.offset == 0.0 && d.interpolation.fract() == 0.0 && (0.0..=4.0).contains(&d.interpolation)
}

impl Event {
    pub fn new(category: i32, d: JudgeData, ctx: Option<NoteCtx>) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let grade_i = d.interpolation.is_finite().then(|| (d.interpolation as i32).clamp(0, 4));
        let synthetic = is_constant(&d);
        let state = match grade_i {
            None => None,
            Some(4) => Some("Miss"),
            Some(_) if synthetic => None,
            Some(_) if d.interpolation < 1.0 => Some("Exact"),
            Some(_) if d.offset < 0.0 => Some("Early"),
            Some(_) => Some("Late"),
        };
        let styles = ctx.map_or(0, |c| c.styles);
        Self {
            kind: "judge",
            seq: SEQ.fetch_add(1, Ordering::Relaxed),
            t_ms: crate::START.get().map_or(0.0, |s| s.elapsed().as_secs_f64() * 1e3),
            cat: cat_name(category),
            grade: grade_i.map_or("Unknown", |g| GRADE[g as usize]),
            grade_i,
            state,
            offset_ms: (!synthetic).then_some(d.offset * 1e3),
            synthetic,
            interp: d.interpolation,
            loc: ctx.map(|c| loc_name(c.location)),
            ex: styles & STYLE_EX != 0,
            fireworks: styles & STYLE_FIREWORKS != 0,
            mine: styles & STYLE_MINE != 0,
            note_cat: ctx.map(|c| cat_name(c.category)),
            hold_len: ctx.filter(|c| c.length > 0.0).map(|c| c.length),
            display_offset_ms: ctx.map(|c| c.timing * 1e3),
        }
    }
}

static TX: OnceLock<SyncSender<Event>> = OnceLock::new();
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn install_sender(tx: SyncSender<Event>) {
    let _ = TX.set(tx);
}

/// Called on the game thread: must never block.
pub fn emit(ev: Event) {
    if let Some(tx) = TX.get() {
        if let Err(TrySendError::Full(_)) = tx.try_send(ev) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locations() {
        assert_eq!(loc_name(0x03), "4");
        assert_eq!(loc_name(0xA0), "A1");
        assert_eq!(loc_name(0xC0), "C1");
        assert_eq!(loc_name(0xE7), "E8");
    }
    #[test]
    fn grades() {
        let e = Event::new(0, JudgeData { interpolation: 2.3, offset: -0.06 }, None);
        assert_eq!((e.grade, e.state, e.cat, e.synthetic), ("Great", Some("Early"), "Tap", false));
        let e = Event::new(1, JudgeData { interpolation: 0.2, offset: 0.01 }, None);
        assert_eq!(e.state, Some("Exact"));
        let e = Event::new(0, JudgeData { interpolation: 1.5, offset: 0.03 }, None);
        assert_eq!(e.state, Some("Late"));
    }
    #[test]
    fn misses_and_constants() {
        // JudgeData.Miss: synthetic, no offset
        let e = Event::new(0, JudgeData { interpolation: 4.0, offset: 0.0 }, None);
        assert_eq!((e.grade, e.state, e.synthetic, e.offset_ms), ("Miss", Some("Miss"), true, None));
        // slide missed with a real (late) offset: keeps it
        let e = Event::new(3, JudgeData { interpolation: 4.0, offset: 0.7105 }, None);
        assert_eq!((e.state, e.synthetic), (Some("Miss"), false));
        assert!((e.offset_ms.unwrap() - 710.5).abs() < 1e-9);
        // JudgeData.Good handed out for an unfinished slide: no direction
        let e = Event::new(3, JudgeData { interpolation: 3.0, offset: 0.0 }, None);
        assert_eq!((e.grade, e.state, e.synthetic, e.offset_ms), ("Good", None, true, None));
        let e = Event::new(0, JudgeData { interpolation: f64::NAN, offset: 0.0 }, None);
        assert_eq!((e.grade, e.state), ("Unknown", None));
    }
}
