//! DG-LAB: an embedded DG-LAB V4 socket relay that is also the controller.
//!
//!   DG-LAB APP --ws://<phone>:<port>/?tid=<tid>--> this lib (relay + controller)
//!   control page (web/index.html) --ws://<phone>:<port>/control--> this lib
//!
//! The APP joins with our fixed controller id (`tid`, kept in dglab.json, so
//! the QR code stays valid across game restarts) and reports its devices.
//! Judgements arrive from the broadcaster thread (`on_event`); a matching rule
//! sends an AppendPulseData `device.op` to every Coyote, so no browser has to
//! stay open. The page only pairs, shows status and edits the settings.
//!
//! Protocol: https://github.com/dungeonlab-open/dglab-kit (README "V4 协议参考").
use crate::events::{self, Event};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Mutex, MutexGuard, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// rule keys in display order; "Break" = a break note judged below Critical Perfect
pub const RULES: [&str; 4] = ["Miss", "Good", "Great", "Break"];

const STORE_PATHS: &[&str] = &[
    "/sdcard/Android/data/com.Reflektone.AstroDX/files/dglab.json",
    "/storage/emulated/0/Android/data/com.Reflektone.AstroDX/files/dglab.json",
    "/data/data/com.Reflektone.AstroDX/files/dglab.json",
];
const LOG_LINES: usize = 60;
const MAX_EXCLUDED: usize = 64;

// ---------------------------------------------------------------- settings

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Rule {
    pub on: bool,
    /// waveform intensity in % (the channel strength itself is set in the APP)
    pub pct: u32,
    pub ms: u32,
    pub wave: String,
}

impl Rule {
    fn new(pct: u32, ms: u32) -> Self {
        Self { on: true, pct, ms, wave: "SHOCK".into() }
    }
}

impl Default for Rule {
    fn default() -> Self {
        Self::new(50, 300)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Rules {
    #[serde(rename = "Miss")]
    pub miss: Rule,
    #[serde(rename = "Good")]
    pub good: Rule,
    #[serde(rename = "Great")]
    pub great: Rule,
    #[serde(rename = "Break")]
    pub brk: Rule,
}

impl Default for Rules {
    fn default() -> Self {
        Self { miss: Rule::new(100, 800), good: Rule::new(70, 500), great: Rule::new(45, 300), brk: Rule::new(60, 400) }
    }
}

impl Rules {
    fn get(&self, key: &str) -> Option<&Rule> {
        match key {
            "Miss" => Some(&self.miss),
            "Good" => Some(&self.good),
            "Great" => Some(&self.great),
            "Break" => Some(&self.brk),
            _ => None,
        }
    }
    fn all_mut(&mut self) -> [&mut Rule; 4] {
        [&mut self.miss, &mut self.good, &mut self.great, &mut self.brk]
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// "A", "B" or "AB"
    pub channels: String,
    /// an equally strong trigger restarts a playing pulse at most this often
    pub min_gap_ms: u32,
    pub rules: Rules,
    /// slotIds that get no pulses
    pub excluded: Vec<String>,
    /// arm the output when the game starts
    pub auto_arm: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { channels: "A".into(), min_gap_ms: 150, rules: Rules::default(), excluded: Vec::new(), auto_arm: false }
    }
}

impl Settings {
    /// Clamps everything into range; unknown waveforms fall back to SHOCK.
    pub fn sanitize(mut self) -> Self {
        if !["A", "B", "AB"].contains(&self.channels.as_str()) {
            self.channels = "A".into();
        }
        self.min_gap_ms = self.min_gap_ms.min(5000);
        for r in self.rules.all_mut() {
            r.pct = r.pct.min(100);
            r.ms = r.ms.clamp(100, 10_000);
            if wave(&r.wave).is_none() {
                r.wave = "SHOCK".into();
            }
        }
        self.excluded.sort();
        self.excluded.dedup();
        self.excluded.truncate(MAX_EXCLUDED);
        self
    }

    fn channel_ids(&self) -> &'static [u8] {
        match self.channels.as_str() {
            "B" => &[1],
            "AB" => &[0, 1],
            _ => &[0],
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Stored {
    tid: String,
    settings: Settings,
}

// ---------------------------------------------------------------- waveforms

#[derive(Deserialize)]
struct Wave {
    id: String,
    name: String,
    raw: Vec<String>,
}

fn waves() -> &'static [Wave] {
    #[derive(Deserialize)]
    struct File {
        waveforms: Vec<Wave>,
    }
    static W: OnceLock<Vec<Wave>> = OnceLock::new();
    W.get_or_init(|| {
        let f: File = serde_json::from_str(include_str!("waveforms.json")).expect("waveforms.json");
        f.waveforms
            .into_iter()
            .filter(|w| !w.raw.is_empty() && w.raw.iter().all(|f| f.len() == 16 && u64::from_str_radix(f, 16).is_ok()))
            .collect()
    })
}

fn wave(id: &str) -> Option<&'static Wave> {
    waves().iter().find(|w| w.id == id)
}

/// V3 pulse frames covering `ms`: the waveform repeated, intensity bytes scaled to `pct` %.
pub fn build_frames(raw: &[String], pct: u32, ms: u32) -> Vec<String> {
    let n = ms.div_ceil(100).max(1) as usize;
    (0..n)
        .map(|i| {
            let f = &raw[i % raw.len()];
            let mut s = f[..8].to_ascii_uppercase();
            for j in (8..16).step_by(2) {
                let v = u32::from_str_radix(&f[j..j + 2], 16).unwrap_or(0);
                let v = ((v * pct + 50) / 100).min(100);
                s.push_str(&format!("{v:02X}"));
            }
            s
        })
        .collect()
}

// ---------------------------------------------------------------- matching

/// Rule keys a judgement matches.
pub fn match_rules(grade: &str, cat: &str, note_cat: Option<&str>) -> Vec<&'static str> {
    let mut out = Vec::new();
    match grade {
        "Miss" => out.push("Miss"),
        "Good" => out.push("Good"),
        "Great" => out.push("Great"),
        _ => {}
    }
    let is_break = cat == "Break" || note_cat == Some("Break");
    if is_break && matches!(grade, "Perfect" | "Great" | "Good" | "Miss") {
        out.push("Break");
    }
    out
}

/// The enabled matching rule with the highest strength (ties: RULES order).
fn pick<'a>(s: &'a Settings, keys: &[&'static str]) -> Option<(&'static str, &'a Rule)> {
    let mut best: Option<(&'static str, &Rule)> = None;
    for k in RULES {
        if !keys.contains(&k) {
            continue;
        }
        let r = s.rules.get(k)?;
        if r.on && r.pct > 0 && best.is_none_or(|(_, b)| r.pct > b.pct) {
            best = Some((k, r));
        }
    }
    best
}

/// A pulse still playing is only replaced by a stronger one, or by an equally
/// strong one after `min_gap`.
#[derive(Default)]
struct Gate {
    until: Option<Instant>,
    pct: u32,
    at: Option<Instant>,
}

impl Gate {
    fn pass(&mut self, pct: u32, ms: u32, now: Instant, min_gap: Duration) -> bool {
        if let (Some(until), Some(at)) = (self.until, self.at) {
            if now < until && (pct < self.pct || (pct == self.pct && now - at < min_gap)) {
                return false;
            }
        }
        self.until = Some(now + Duration::from_millis(ms as u64));
        self.pct = pct;
        self.at = Some(now);
        true
    }
}

// ---------------------------------------------------------------- device list

fn deep_merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                match b.get_mut(k) {
                    Some(bv) if bv.is_object() && v.is_object() => deep_merge(bv, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn slot_id(d: &Value) -> Option<&str> {
    d.get("slotId").and_then(Value::as_str)
}

/// Applies devices.snapshot / devices.patch / slots.patch; false if `ev` is none of them.
pub fn apply_device_event(devices: &mut Vec<Value>, ev: &Value) -> bool {
    let list = |v: Option<&Value>| -> Vec<Value> {
        v.and_then(Value::as_array).map_or_else(Vec::new, |a| a.iter().filter(|d| slot_id(d).is_some()).cloned().collect())
    };
    match ev.get("ev").and_then(Value::as_str) {
        Some("devices.snapshot") => *devices = list(ev.get("devices")),
        Some("devices.patch") => {
            let added = list(ev.get("added"));
            let removed: Vec<&str> = ev.get("removed").and_then(Value::as_array).map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).collect());
            devices.retain(|d| {
                let id = slot_id(d).unwrap_or("");
                !removed.contains(&id) && !added.iter().any(|a| slot_id(a) == Some(id))
            });
            devices.extend(added);
        }
        Some("slots.patch") => {
            for p in list(ev.get("slots")) {
                let Some(d) = devices.iter_mut().find(|d| slot_id(d) == slot_id(&p)) else { continue };
                for key in ["props", "slotState"] {
                    if let Some(pv) = p.get(key) {
                        match d.get_mut(key) {
                            Some(dv) => deep_merge(dv, pv),
                            None => {
                                d[key] = pv.clone();
                            }
                        }
                    }
                }
            }
        }
        _ => return false,
    }
    true
}

/// Takes Coyote pulse frames and is not known to be disconnected.
pub fn is_pulse_device(d: &Value) -> bool {
    d.get("type").and_then(Value::as_str).is_some_and(|t| t.starts_with("COYOTE"))
        && d.pointer("/slotState/hasDevice") != Some(&Value::Bool(false))
}

// ---------------------------------------------------------------- state

struct App {
    conn: u64,
    id: String,
    peer: String,
    tx: Sender<String>,
    devices: Vec<Value>,
}

struct State {
    tid: String,
    port: u16,
    settings: Settings,
    armed: bool,
    gate: Gate,
    apps: Vec<App>,
    controls: Vec<(u64, Sender<String>)>,
    fired: [u64; 4],
    log: VecDeque<Value>,
    next_conn: u64,
    next_req: u64,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(load()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// 128 random bits as a UUID v4 string.
fn new_id() -> String {
    let mut b = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_ok();
    if !ok {
        use std::hash::{BuildHasher, Hasher};
        for (i, c) in b.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(unix_ms() ^ i as u64);
            c.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn load() -> State {
    let stored = STORE_PATHS
        .iter()
        .find_map(|p| {
            let s = std::fs::read_to_string(p).ok()?;
            match serde_json::from_str::<Stored>(&s) {
                Ok(v) => {
                    log::info!("dglab settings loaded from {p}");
                    Some(v)
                }
                Err(e) => {
                    log::warn!("bad {p}: {e}");
                    None
                }
            }
        })
        .unwrap_or_default();
    let fresh = stored.tid.is_empty();
    let settings = stored.settings.sanitize();
    let st = State {
        tid: if fresh { new_id() } else { stored.tid },
        port: 0,
        armed: settings.auto_arm,
        settings,
        gate: Gate::default(),
        apps: Vec::new(),
        controls: Vec::new(),
        fired: [0; 4],
        log: VecDeque::new(),
        next_conn: 0,
        next_req: 0,
    };
    if fresh {
        save(&st);
    }
    st
}

fn save(st: &State) {
    let text = match serde_json::to_string_pretty(&json!({ "tid": st.tid, "settings": st.settings })) {
        Ok(t) => t,
        Err(_) => return,
    };
    for p in STORE_PATHS {
        if std::fs::write(p, &text).is_ok() {
            return;
        }
    }
    log::warn!("could not save dglab.json; settings are kept until the game exits");
}

/// Called once at start-up with the port the server listens on.
pub fn init(port: u16) {
    let mut st = state();
    st.port = port;
    log::info!("DG-LAB relay: APP joins with ws://<phone ip>:{port}/?tid={}", st.tid);
}

impl State {
    fn log(&mut self, msg: String) {
        log::info!("dglab: {msg}");
        let line = json!({ "t": unix_ms(), "msg": msg });
        let ev = json!({ "type": "log", "line": line }).to_string();
        self.controls.retain(|(_, tx)| tx.send(ev.clone()).is_ok());
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn status(&self, with_log: bool) -> String {
        let apps: Vec<Value> = self.apps.iter().map(|a| json!({ "id": a.id, "peer": a.peer, "devices": a.devices })).collect();
        let fired: serde_json::Map<String, Value> = RULES.iter().zip(self.fired).map(|(k, n)| ((*k).to_string(), json!(n))).collect();
        let mut v = json!({
            "type": "status",
            "tid": self.tid,
            "port": self.port,
            "addrs": local_ipv4s(),
            "armed": self.armed,
            "settings": self.settings,
            "waves": waves().iter().map(|w| json!({ "id": w.id, "name": w.name })).collect::<Vec<_>>(),
            "apps": apps,
            "fired": fired,
            "song": events::current_song(),
            "hooked": crate::ws::hooked(),
        });
        if with_log {
            v["log"] = json!(self.log);
        }
        v.to_string()
    }

    fn push_status(&mut self) {
        if self.controls.is_empty() {
            return;
        }
        let s = self.status(false);
        self.controls.retain(|(_, tx)| tx.send(s.clone()).is_ok());
    }

    /// Queues an RPC request for one APP; false if its connection is gone.
    fn request(&mut self, app: usize, method: &str, data: Option<Value>) -> bool {
        self.next_req += 1;
        let mut req = json!({ "t": "req", "reqId": format!("ap-{}", self.next_req), "m": method });
        if let Some(d) = data {
            req["data"] = d;
        }
        // relayed frames name the other end in `clientId`: for the APP that is us
        let frame = json!({ "type": "message", "clientId": self.tid, "data": req }).to_string();
        self.apps[app].tx.send(frame).is_ok()
    }

    fn fire(&mut self, key: &'static str, why: &str) {
        let Some(rule) = self.settings.rules.get(key).cloned() else { return };
        let Some(w) = wave(&rule.wave).or_else(|| wave("SHOCK")) else { return };
        let frames = build_frames(&w.raw, rule.pct, rule.ms);
        let d = frames.len() as u32 * 100;
        let mut sent = 0;
        for i in 0..self.apps.len() {
            let slots: Vec<String> = self.apps[i]
                .devices
                .iter()
                .filter(|d| is_pulse_device(d))
                .filter_map(|d| slot_id(d).map(str::to_owned))
                .filter(|s| !self.settings.excluded.contains(s))
                .collect();
            for s in slots {
                for &c in self.settings.channel_ids() {
                    let op = json!({ "s": s, "t": 0, "c": c, "p": 1, "d": d, "v": frames, "im": true });
                    if self.request(i, "device.op", Some(op)) {
                        sent += 1;
                    }
                }
            }
        }
        if let Some(i) = RULES.iter().position(|k| *k == key) {
            self.fired[i] += 1;
        }
        let ev = json!({ "type": "fired", "rule": key, "pct": rule.pct, "ms": rule.ms, "sent": sent, "count": self.fired }).to_string();
        self.controls.retain(|(_, tx)| tx.send(ev.clone()).is_ok());
        if sent == 0 {
            self.log(format!("{why}：没有可输出的郊狼"));
        }
    }

    fn stop(&mut self, zero: bool) {
        self.armed = false;
        self.gate = Gate::default();
        for i in 0..self.apps.len() {
            self.request(i, "device.op.clear", None);
            if !zero {
                continue;
            }
            let slots: Vec<String> =
                self.apps[i].devices.iter().filter(|d| is_pulse_device(d)).filter_map(|d| slot_id(d).map(str::to_owned)).collect();
            for s in slots {
                for c in [0, 1] {
                    self.request(i, "device.op", Some(json!({ "s": s, "t": 7, "c": c, "p": 1, "v": 0 })));
                }
            }
        }
        self.log(if zero { "已停止输出并将强度归零".into() } else { "已停止输出".into() });
        self.push_status();
    }
}

// ---------------------------------------------------------------- game events (broadcaster thread)

pub fn on_event(ev: &Event) {
    let mut st = state();
    match ev {
        Event::Judge(j) => {
            if !st.armed || st.apps.is_empty() {
                return;
            }
            let keys = match_rules(j.grade, j.cat, j.note_cat);
            let Some((key, rule)) = pick(&st.settings, &keys) else { return };
            let (pct, ms) = (rule.pct, rule.ms);
            let gap = Duration::from_millis(st.settings.min_gap_ms as u64);
            if st.gate.pass(pct, ms, Instant::now(), gap) {
                let why = format!("{} {}", j.cat, j.grade);
                st.fire(key, &why);
            }
        }
        Event::SongStart(_) | Event::SongEnd(_) => st.push_status(),
    }
}

// ---------------------------------------------------------------- APP connections (relay side)

/// `?tid=...` of a request path, if any.
pub fn tid_of(path: &str) -> Option<&str> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.strip_prefix("tid="))
}

pub struct Conn {
    pub id: u64,
    pub rx: Receiver<String>,
}

/// An APP connected with `tid`: registers it and queues the relay greeting.
/// Err(frame) = wrong tid; send that frame and close with 4001.
pub fn app_connect(tid: &str, peer: &str) -> Result<Conn, String> {
    let mut st = state();
    if tid != st.tid {
        return Err(json!({ "type": "error", "code": "controller_not_found", "message": "controller_not_found" }).to_string());
    }
    let (tx, rx) = channel();
    st.next_conn += 1;
    let conn = st.next_conn;
    let id = new_id();
    let _ = tx.send(json!({ "type": "hello", "clientId": id }).to_string());
    let _ = tx.send(json!({ "type": "controller_attached", "clientId": st.tid }).to_string());
    st.apps.push(App { conn, id: id.clone(), peer: peer.to_owned(), tx, devices: Vec::new() });
    st.log(format!("APP 已连接（{peer}）"));
    st.push_status();
    Ok(Conn { id: conn, rx })
}

pub fn app_text(conn: u64, text: &str) {
    let Ok(f) = serde_json::from_str::<Value>(text) else { return };
    let mut st = state();
    let Some(i) = st.apps.iter().position(|a| a.conn == conn) else { return };
    match f.get("type").and_then(Value::as_str) {
        Some("ping") => {
            let _ = st.apps[i].tx.send(json!({ "type": "pong", "ts": unix_ms() }).to_string());
        }
        Some("message") => {
            let data = f.get("data").cloned().unwrap_or(Value::Null);
            match data.get("t").and_then(Value::as_str) {
                Some("resp") => {
                    if let Some(e) = data.get("error") {
                        let id = data.get("reqId").cloned().unwrap_or(Value::Null);
                        st.log(format!("APP 拒绝了指令 {id}：{e}"));
                    }
                }
                Some("ev") if data.get("ev").and_then(Value::as_str) == Some("custom.action") => {
                    let n = data.get("action").cloned().unwrap_or(Value::Null);
                    st.log(format!("APP 按下了自定义动作 {n}"));
                }
                Some("ev") => {
                    if apply_device_event(&mut st.apps[i].devices, &data) {
                        st.push_status();
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

pub fn app_disconnect(conn: u64) {
    let mut st = state();
    if let Some(i) = st.apps.iter().position(|a| a.conn == conn) {
        let a = st.apps.remove(i);
        st.log(format!("APP 已断开（{}）", a.peer));
        st.push_status();
    }
}

// ---------------------------------------------------------------- control page

pub fn control_connect() -> Conn {
    let mut st = state();
    let (tx, rx) = channel();
    st.next_conn += 1;
    let id = st.next_conn;
    let _ = tx.send(st.status(true));
    st.controls.push((id, tx));
    Conn { id, rx }
}

pub fn control_disconnect(id: u64) {
    state().controls.retain(|(c, _)| *c != id);
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Command {
    Settings { settings: Settings },
    Arm { on: bool },
    Test { rule: String },
    Stop { zero: bool },
}

pub fn control_text(text: &str) {
    let cmd = match serde_json::from_str::<Command>(text) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("bad control command: {e}");
            return;
        }
    };
    let mut st = state();
    match cmd {
        Command::Settings { settings } => {
            st.settings = settings.sanitize();
            save(&st);
            st.push_status();
        }
        Command::Arm { on } => {
            if st.armed != on {
                st.armed = on;
                st.gate = Gate::default();
                st.log(if on { "输出已开启".into() } else { "输出已关闭".into() });
            }
            st.push_status();
        }
        Command::Test { rule } => {
            if let Some(key) = RULES.iter().copied().find(|k| *k == rule) {
                st.gate = Gate::default();
                st.fire(key, "测试");
            }
        }
        Command::Stop { zero } => st.stop(zero),
    }
}

// ---------------------------------------------------------------- misc

/// Non-loopback IPv4 addresses, for the APP address shown on the page.
fn local_ipv4s() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut ifs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifs) != 0 {
            return out;
        }
        let mut p = ifs;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() && !ip.is_unspecified() && !out.contains(&ip.to_string()) {
                    out.push(ip.to_string());
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifs);
    }
    out
}
