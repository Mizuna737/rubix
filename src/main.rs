#![allow(irrefutable_let_patterns)]

mod handlers;

mod config;
mod grabs;
mod input;
mod model;
mod state;
mod udev;
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

/// Which rendering/input backend to drive.
enum Backend {
    /// Nested window under a host compositor (winit) -- dev/testing.
    Winit,
    /// Direct DRM/KMS + libinput on a bare TTY (Track B).
    Udev,
}

/// Pick a backend. An explicit `RUBIX_BACKEND=winit|udev|tty` wins; otherwise run
/// nested when a host display is present (`WAYLAND_DISPLAY`/`DISPLAY` set) and drive
/// the TTY when it isn't. Detection runs before `init_winit` clobbers
/// `WAYLAND_DISPLAY` with our own socket, so it sees the host environment.
fn detect_backend() -> Backend {
    match std::env::var("RUBIX_BACKEND").as_deref() {
        Ok("winit") => Backend::Winit,
        Ok("udev") | Ok("tty") => Backend::Udev,
        _ => {
            if std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("DISPLAY").is_some()
            {
                Backend::Winit
            } else {
                Backend::Udev
            }
        }
    }
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

/// Open `$XDG_CACHE_HOME/rubix/rubix.log` (or `~/.cache/rubix/rubix.log`),
/// creating the directory. Returns `None` if neither env var is set or the file
/// can't be created -- logging then falls back to stderr only. On the TTY backend
/// stderr scrolls off an unreachable console, so the file is the only way to see
/// what happened; this makes a log exist without the user having to redirect.
fn open_log_file() -> Option<std::fs::File> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))?
        .join("rubix");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::File::create(dir.join("rubix.log")).ok()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Always tee to a file (survives the TTY), plus stderr for nested dev runs.
    match open_log_file() {
        Some(file) => {
            let writer = std::sync::Mutex::new(file).and(std::io::stderr);
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(writer)
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
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

    match detect_backend() {
        Backend::Winit => {
            tracing::info!("starting winit (nested) backend");
            crate::winit::init_winit(&mut event_loop, &mut data)?;
        }
        Backend::Udev => {
            tracing::info!("starting udev (TTY/DRM) backend");
            crate::udev::init_udev(&mut event_loop, &mut data)?;
        }
    }

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

    // After every dispatch cycle: refresh the space (enter/leave bookkeeping),
    // clean up dead popups, and -- critically -- flush queued events out to
    // clients. Without the flush, a client's opening registry roundtrip never
    // completes, so it blocks before ever creating a window (the winit backend
    // got away with flushing only in its Redraw handler; the udev render loop
    // doesn't, so this backend-neutral flush is what lets clients start at all).
    event_loop.run(None, &mut data, move |data| {
        data.state.space.refresh();
        data.state.popups.cleanup();
        let _ = data.display_handle.flush_clients();
    })?;

    Ok(())
}
