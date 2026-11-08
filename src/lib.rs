//! libjudgehook.so — loaded via a DT_NEEDED entry added to libmain.so.
//!
//! Load sequence:
//!   .init_array (`on_load`)  -> spawn the bootstrap thread, return immediately
//!   bootstrap thread         -> start WebSocket server
//!                            -> wait for libil2cpp.so + il2cpp runtime init
//!                            -> resolve methods by name, install inline hooks
//!   game main thread         -> hooks push `Judgement` events into a bounded queue
//!   broadcaster thread       -> serialise to JSON, send to every WS client

// host builds only compile the pure-Rust parts for `cargo test`
#![cfg_attr(not(target_os = "android"), allow(dead_code, unused_imports))]

mod config;
mod events;
#[cfg(target_os = "android")]
mod hooks;
#[cfg(target_os = "android")]
mod il2cpp;
mod ws;

use std::{sync::OnceLock, thread, time::Instant};

pub(crate) static START: OnceLock<Instant> = OnceLock::new();

#[cfg(target_os = "android")]
extern "C" fn on_load() {
    // Nothing that can block or panic here: we run inside the dynamic linker.
    START.get_or_init(Instant::now);
    let _ = thread::Builder::new()
        .name("judgehook-init".into())
        .spawn(|| {
            android_logger::init_once(
                android_logger::Config::default()
                    .with_max_level(log::LevelFilter::Info)
                    .with_tag("JudgeHook"),
            );
            if let Err(p) = std::panic::catch_unwind(bootstrap) {
                log::error!("bootstrap panicked: {:?}", p.downcast_ref::<&str>());
            }
        });
}

#[cfg(target_os = "android")]
fn bootstrap() {
    log::info!("libjudgehook {} loaded", env!("CARGO_PKG_VERSION"));
    let cfg = config::load();
    let tx = ws::start(&cfg);
    events::install_sender(tx);

    match il2cpp::wait_and_resolve() {
        Ok(targets) => match hooks::install(&targets) {
            Ok(()) => {
                ws::set_hooked(true);
                log::info!("hooks installed, judgements will be streamed");
            }
            Err(e) => log::error!("hook install failed: {e}"),
        },
        Err(e) => log::error!("il2cpp resolve failed: {e}"),
    }
}

#[cfg(target_os = "android")]
#[used]
#[link_section = ".init_array"]
static INIT_ARRAY: extern "C" fn() = on_load;
