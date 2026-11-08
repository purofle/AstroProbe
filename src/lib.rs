//! libjudgehook.so — loaded via a DT_NEEDED entry added to libmain.so.
//!
//! Load sequence:
//!   .init_array (`on_load`)   -> redirect libmain.so's `dlopen` GOT slot (got.rs),
//!                                spawn the WebSocket thread, return immediately
//!   NativeLoader.load (Java)  -> libmain dlopens libil2cpp.so -> our detour
//!                                inline-hooks `il2cpp_init`
//!   libunity: il2cpp_init     -> original runs, then (same thread) resolve the
//!                                game methods by name and install their hooks
//!   game main thread          -> hooks push `Judgement` events into a bounded queue
//!   broadcaster thread        -> serialise to JSON, send to every WS client
//!
//! If the GOT hook cannot be installed, the init thread falls back to polling
//! for libil2cpp.so and hooks `il2cpp_init` from there.

// host builds only compile the pure-Rust parts for `cargo test`
#![cfg_attr(not(target_os = "android"), allow(dead_code, unused_imports))]

mod config;
mod events;
#[cfg(target_os = "android")]
mod got;
#[cfg(target_os = "android")]
mod hooks;
#[cfg(target_os = "android")]
mod il2cpp;
#[cfg(target_os = "android")]
mod inline;
mod ws;

use std::{
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

pub(crate) static START: OnceLock<Instant> = OnceLock::new();

pub(crate) fn since_load() -> Duration {
    START.get().map_or(Duration::ZERO, |s| s.elapsed())
}

/// `name` is `lib` or ends with `/lib` (also covers `base.apk!/lib/<abi>/lib`)
pub(crate) fn is_lib(name: &[u8], lib: &[u8]) -> bool {
    name == lib || (name.len() > lib.len() && name.ends_with(lib) && name[name.len() - lib.len() - 1] == b'/')
}

#[cfg(target_os = "android")]
extern "C" fn on_load() {
    // We run inside the dynamic linker (libmain.so is still being loaded):
    // nothing here may block, and a panic must not unwind into C.
    START.get_or_init(Instant::now);
    let _ = std::panic::catch_unwind(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("JudgeHook"),
        );
        log::info!("libjudgehook {} loaded", env!("CARGO_PKG_VERSION"));
        let got_hooked = match got::hook_libmain_dlopen() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("libmain dlopen GOT hook failed: {e}; will poll for libil2cpp.so");
                false
            }
        };
        let _ = thread::Builder::new()
            .name("judgehook-init".into())
            .spawn(move || {
                if let Err(p) = std::panic::catch_unwind(|| bootstrap(got_hooked)) {
                    log::error!("bootstrap panicked: {:?}", p.downcast_ref::<&str>());
                }
            });
    });
}

#[cfg(target_os = "android")]
fn bootstrap(got_hooked: bool) {
    if !got_hooked {
        // own thread: il2cpp_init must not slip past us while the WS server starts
        let _ = thread::Builder::new()
            .name("judgehook-poll".into())
            .spawn(|| il2cpp::poll_for_lib(Duration::from_secs(120)));
    }
    let cfg = config::load();
    let tx = ws::start(&cfg);
    events::install_sender(tx);
}

#[cfg(target_os = "android")]
#[used]
#[link_section = ".init_array"]
static INIT_ARRAY: extern "C" fn() = on_load;

#[cfg(test)]
mod tests {
    use super::is_lib;

    #[test]
    fn lib_name_matching() {
        assert!(is_lib(b"libil2cpp.so", b"libil2cpp.so"));
        assert!(is_lib(b"/data/app/x/lib/arm64/libil2cpp.so", b"libil2cpp.so"));
        assert!(is_lib(b"/data/app/x/base.apk!/lib/arm64-v8a/libmain.so", b"libmain.so"));
        assert!(!is_lib(b"/data/app/x/lib/arm64/foo_libil2cpp.so", b"libil2cpp.so"));
        assert!(!is_lib(b"libil2cpp.so.bak", b"libil2cpp.so"));
        assert!(!is_lib(b"bil2cpp.so", b"libil2cpp.so"));
    }
}
