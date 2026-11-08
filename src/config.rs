//! Optional config: /sdcard/Android/data/com.Reflektone.AstroDX/files/judgehook.json
//!   { "bind": "0.0.0.0", "port": 8765, "web": true }
//! The app can read its own external files dir without any permission.
use serde::Deserialize;

const PATHS: &[&str] = &[
    "/sdcard/Android/data/com.Reflektone.AstroDX/files/judgehook.json",
    "/storage/emulated/0/Android/data/com.Reflektone.AstroDX/files/judgehook.json",
];

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    /// max queued events before new ones are dropped (never blocks the game)
    pub queue: usize,
    /// DG-LAB: the control page, the embedded relay the APP joins and the
    /// judgement -> pulse logic, all on the same port (dglab.rs)
    pub web: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { bind: "0.0.0.0".into(), port: 8765, queue: 4096, web: true }
    }
}

pub fn load() -> Config {
    for p in PATHS {
        match std::fs::read_to_string(p) {
            Ok(s) => match serde_json::from_str::<Config>(&s) {
                Ok(c) => {
                    log::info!("config loaded from {p}: {c:?}");
                    return c;
                }
                Err(e) => log::warn!("bad config {p}: {e}, using defaults"),
            },
            Err(_) => continue,
        }
    }
    Config::default()
}
