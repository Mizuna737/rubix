#![allow(irrefutable_let_patterns)]

mod handlers;

mod config;
mod grabs;
mod input;
mod model;
mod state;
mod winit;

use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::{Display, DisplayHandle},
};
pub use state::RubixState;

pub struct CalloopData {
    state: RubixState,
    display_handle: DisplayHandle,
}

/// Watch the user config file for live keybind reload. Best-effort: any failure
/// (no config dir, watcher creation, or registering the watch) logs and leaves
/// the compositor running with its startup binds -- hot-reload is a convenience,
/// never a hard dependency.
fn init_config_watch(event_loop: &EventLoop<CalloopData>) {
    use calloop_notify::notify::{RecursiveMode, Watcher};
    use calloop_notify::NotifySource;

    let Some((dir, file_name)) = crate::config::config_watch_target() else {
        tracing::debug!("no user config to watch; hot-reload inactive");
        return;
    };

    let mut source = match NotifySource::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("could not create config watcher ({e}); hot-reload disabled");
            return;
        }
    };

    if let Err(e) = source.watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!("config watch failed on {dir:?} ({e}); hot-reload disabled");
        return;
    }

    let registered = event_loop.handle().insert_source(source, move |event, _, data| {
        // Logged before filtering so a live save shows the raw kind + paths --
        // the ground truth for tuning `should_reload` (run with RUST_LOG=rubix=debug).
        tracing::debug!("fs event: {event:?}");
        if crate::config::should_reload(&event, &file_name) {
            data.state.reload_config();
        }
    });

    match registered {
        Ok(_) => tracing::info!("watching {dir:?} for config changes"),
        Err(e) => tracing::warn!("failed to register config watch source ({e}); hot-reload disabled"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;

    let display: Display<RubixState> = Display::new()?;
    let display_handle = display.handle();
    let config = crate::config::Config::load();
    let state = RubixState::new(&mut event_loop, display, config);

    let mut data = CalloopData {
        state,
        display_handle,
    };

    crate::winit::init_winit(&mut event_loop, &mut data)?;

    init_config_watch(&event_loop);

    let mut args = std::env::args().skip(1);
    let flag = args.next();
    let arg = args.next();

    match (flag.as_deref(), arg) {
        (Some("-c") | Some("--command"), Some(command)) => {
            std::process::Command::new(command).spawn().ok();
        }
        _ => {
            std::process::Command::new("alacritty").spawn().ok();
        }
    }

    event_loop.run(None, &mut data, move |_| {
        // RubixState is running
    })?;

    Ok(())
}
