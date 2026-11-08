//! Event model (judgements, song start/end) + the non-blocking hand-off from
//! the game thread.
use serde::Serialize;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, TrySendError},
    Mutex, OnceLock,
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

/// Everything sent to clients; serialised without a wrapper (each variant has `type`).
#[derive(Serialize)]
#[serde(untagged)]
pub enum Event {
    Judge(Judge),
    SongStart(SongStart),
    SongEnd(SongEnd),
}

#[derive(Serialize)]
pub struct Judge {
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u64,
    /// id of the song_start this judgement belongs to (0 = none seen)
    play: u64,
    /// ms since the hook library was loaded (monotonic)
    t_ms: f64,
    pub(crate) cat: &'static str,
    pub(crate) grade: &'static str,
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
    pub(crate) note_cat: Option<&'static str>,
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

fn now_ms() -> f64 {
    crate::START.get().map_or(0.0, |s| s.elapsed().as_secs_f64() * 1e3)
}

impl Judge {
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
            play: PLAY.load(Ordering::Relaxed),
            t_ms: now_ms(),
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

/// What is being played, read from DataSystem / Difficulty / Level when the
/// gameplay scene initialises.
#[derive(Serialize, Clone, Debug, Default)]
pub struct SongInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    /// display name, e.g. "Master", "Re:Master" (Difficulty.Alias)
    pub difficulty: Option<String>,
    /// simai chart index: &inote_<id>
    pub difficulty_id: i32,
    /// level text from the chart, e.g. "13.4", "12.7?"
    pub level: Option<String>,
    pub charter: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct SongStart {
    #[serde(rename = "type")]
    kind: &'static str,
    play: u64,
    t_ms: f64,
    #[serde(flatten)]
    song: SongInfo,
}

#[derive(Serialize, Default, Clone, Copy)]
#[serde(rename_all = "PascalCase")]
pub struct GradeCounts {
    pub critical_perfect: u64,
    pub perfect: u64,
    pub great: u64,
    pub good: u64,
    pub miss: u64,
}

#[derive(Serialize)]
pub struct SongEnd {
    #[serde(rename = "type")]
    kind: &'static str,
    play: u64,
    t_ms: f64,
    /// "finish" (played to the end), "restart", "quit", or "unknown" (the next
    /// song started without an end being seen)
    reason: &'static str,
    /// ms since this play's song_start
    duration_ms: f64,
    /// judgements sent for this play
    judged: u64,
    grades: GradeCounts,
}

static TX: OnceLock<SyncSender<Event>> = OnceLock::new();
pub static DROPPED: AtomicU64 = AtomicU64::new(0);
/// current play id (incremented by every song_start)
static PLAY: AtomicU64 = AtomicU64::new(0);
/// per-grade judgement counts of the current play (index = grade_i)
static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// the song_start of the play in progress; None between plays
static CURRENT: Mutex<Option<SongStart>> = Mutex::new(None);

pub fn install_sender(tx: SyncSender<Event>) {
    let _ = TX.set(tx);
}

fn send(ev: Event) {
    if let Some(tx) = TX.get() {
        if let Err(TrySendError::Full(_)) = tx.try_send(ev) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Called on the game thread: must never block.
pub fn emit(j: Judge) {
    if let Some(g) = j.grade_i {
        COUNTS[g as usize].fetch_add(1, Ordering::Relaxed);
    }
    send(Event::Judge(j));
}

/// The gameplay scene was set up for `song`. A play still open (its end was
/// not seen) is closed first, so every song_start is paired with a song_end.
pub fn song_start(song: SongInfo) {
    song_end("unknown");
    let play = PLAY.fetch_add(1, Ordering::Relaxed) + 1;
    for c in &COUNTS {
        c.store(0, Ordering::Relaxed);
    }
    let ev = SongStart { kind: "song_start", play, t_ms: now_ms(), song };
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(ev.clone());
    send(Event::SongStart(ev));
}

/// The play in progress ended; no-op if none is.
pub fn song_end(reason: &'static str) {
    let Some(start) = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let c = |i: usize| COUNTS[i].load(Ordering::Relaxed);
    let grades = GradeCounts { critical_perfect: c(0), perfect: c(1), great: c(2), good: c(3), miss: c(4) };
    let t_ms = now_ms();
    send(Event::SongEnd(SongEnd {
        kind: "song_end",
        play: start.play,
        t_ms,
        reason,
        duration_ms: t_ms - start.t_ms,
        judged: (0..5).map(c).sum(),
        grades,
    }));
}

/// For the hello message: the song being played right now, if any.
pub fn current_song() -> Option<serde_json::Value> {
    let cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    cur.as_ref().and_then(|s| serde_json::to_value(s).ok())
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
        let e = Judge::new(0, JudgeData { interpolation: 2.3, offset: -0.06 }, None);
        assert_eq!((e.grade, e.state, e.cat, e.synthetic), ("Great", Some("Early"), "Tap", false));
        let e = Judge::new(1, JudgeData { interpolation: 0.2, offset: 0.01 }, None);
        assert_eq!(e.state, Some("Exact"));
        let e = Judge::new(0, JudgeData { interpolation: 1.5, offset: 0.03 }, None);
        assert_eq!(e.state, Some("Late"));
    }
    #[test]
    fn misses_and_constants() {
        // JudgeData.Miss: synthetic, no offset
        let e = Judge::new(0, JudgeData { interpolation: 4.0, offset: 0.0 }, None);
        assert_eq!((e.grade, e.state, e.synthetic, e.offset_ms), ("Miss", Some("Miss"), true, None));
        // slide missed with a real (late) offset: keeps it
        let e = Judge::new(3, JudgeData { interpolation: 4.0, offset: 0.7105 }, None);
        assert_eq!((e.state, e.synthetic), (Some("Miss"), false));
        assert!((e.offset_ms.unwrap() - 710.5).abs() < 1e-9);
        // JudgeData.Good handed out for an unfinished slide: no direction
        let e = Judge::new(3, JudgeData { interpolation: 3.0, offset: 0.0 }, None);
        assert_eq!((e.grade, e.state, e.synthetic, e.offset_ms), ("Good", None, true, None));
        let e = Judge::new(0, JudgeData { interpolation: f64::NAN, offset: 0.0 }, None);
        assert_eq!((e.grade, e.state), ("Unknown", None));
    }

    #[test]
    fn song_lifecycle() {
        // no sender installed: send() is a no-op, the state machine still runs
        song_end("quit"); // nothing open: no-op
        assert!(current_song().is_none());
        let info = SongInfo { title: Some("系ぎて".into()), difficulty: Some("Re:Master".into()),
            difficulty_id: 6, level: Some("15.0".into()), ..Default::default() };
        song_start(info.clone());
        let cur = current_song().unwrap();
        assert_eq!((cur["type"].as_str(), cur["title"].as_str(), cur["difficulty_id"].as_i64()),
            (Some("song_start"), Some("系ぎて"), Some(6)));
        emit(Judge::new(0, JudgeData { interpolation: 0.2, offset: 0.001 }, None));
        emit(Judge::new(0, JudgeData { interpolation: 4.0, offset: 0.0 }, None));
        assert_eq!((COUNTS[0].load(Ordering::Relaxed), COUNTS[4].load(Ordering::Relaxed)), (1, 1));
        song_end("finish");
        assert!(current_song().is_none());
        // song_end serialisation
        let e = SongEnd { kind: "song_end", play: 1, t_ms: 2.0, reason: "finish", duration_ms: 1.0, judged: 2,
            grades: GradeCounts { critical_perfect: 1, miss: 1, ..Default::default() } };
        let v = serde_json::to_value(Event::SongEnd(e)).unwrap();
        assert_eq!((v["type"].as_str(), v["grades"]["CriticalPerfect"].as_u64(), v["grades"]["Miss"].as_u64()),
            (Some("song_end"), Some(1), Some(1)));
        // judge events are serialised flat, unchanged apart from `play`
        let v = serde_json::to_value(Event::Judge(Judge::new(0, JudgeData { interpolation: 1.5, offset: 0.03 }, None))).unwrap();
        assert_eq!((v["type"].as_str(), v["grade"].as_str()), (Some("judge"), Some("Perfect")));
        assert!(v.get("play").is_some());
    }
}
