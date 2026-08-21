#![allow(irrefutable_let_patterns)]

mod handlers;

mod color_management;
mod config;
mod cursor;
mod decoration;
mod edid;
mod wallpaper;
mod focus;
mod grabs;
mod hdr;
mod hdr_shaders;
mod input;
mod ipc;
mod model;
mod output_power;
mod portal;
mod rounding;
mod foreign_toplevel;
mod screencopy;
mod state;
mod udev;
mod winit;

use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::{Display, DisplayHandle},
};
use smithay::xwayland::{X11Wm, XWayland, XWaylandEvent};
use std::process::Stdio;
pub use state::RubixState;

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
fn init_config_watch(event_loop: &EventLoop<RubixState>) {
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
            // Debounced rather than reloaded inline -- one save produces a
            // burst of events, the first of which usually sees a truncated
            // file. See `RubixState::schedule_config_reload`.
            data.schedule_config_reload();
        }
    });

    match registered {
        Ok(_) => tracing::info!("watching {dir:?} for config changes"),
        Err(e) => tracing::warn!("failed to register config watch source ({e}); hot-reload disabled"),
    }
}

/// Spawn XWayland and wire up the `X11Wm` once it signals readiness.
/// Best-effort: any failure to spawn just logs and leaves X11 app support
/// off for this run -- XWayland is a convenience layer, never a hard
/// dependency for the wayland-native compositor to start.
fn init_xwayland(event_loop: &EventLoop<'static, RubixState>, display_handle: &DisplayHandle) {
    let (xwayland, xclient) = match XWayland::spawn(
        display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        std::iter::empty::<String>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| {},
    ) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("failed to spawn XWayland ({e}); X11 app support disabled");
            return;
        }
    };

    let loop_handle = event_loop.handle();
    let display_handle = display_handle.clone();
    let registered = event_loop.handle().insert_source(xwayland, move |event, _, data| match event {
        XWaylandEvent::Ready { x11_socket, display_number } => {
            match X11Wm::start_wm(loop_handle.clone(), &display_handle, x11_socket, xclient.clone()) {
                Ok(wm) => {
                    data.xwm = Some(wm);
                    // Stored, not exported: `Ready` fires mid-event-loop with the
                    // libinput/render threads alive, so a global set_var here would
                    // be UB (edition 2024 marks it unsafe for exactly that reason).
                    // Spawned clients get DISPLAY set explicitly from xdisplay at
                    // spawn time (see NavAction::Spawn in input.rs).
                    data.xdisplay = Some(display_number);
                    tracing::info!("XWayland ready on :{display_number}");

                    // Fire configured startup commands once, now that the
                    // compositor is fully up: the Wayland socket is published
                    // (children inherit WAYLAND_DISPLAY) and XWayland is ready
                    // (DISPLAY set explicitly, as in NavAction::Spawn). `Ready`
                    // is a one-shot event, so this runs exactly once.
                    for command in &data.config.startup {
                        let mut cmd = std::process::Command::new("sh");
                        cmd.arg("-c").arg(command);
                        cmd.env("DISPLAY", format!(":{display_number}"));
                        cmd.spawn().ok();
                        tracing::info!("ran startup command: {command}");
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to start X11 WM on XWayland :{display_number} ({e})");
                }
            }
        }
        XWaylandEvent::Error => {
            tracing::warn!("XWayland failed to start");
        }
    });

    match registered {
        Ok(_) => tracing::info!("XWayland spawn requested"),
        Err(e) => tracing::warn!("failed to register XWayland source ({e}); X11 app support disabled"),
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

// `rubix screen ...` -- a tight `hyprctl`/`niri msg`-style client for
// `zwlr_output_power_v1` (via `ipc::Request::SetScreenPower`/`ScreenStatus`),
// NOT a general CLI framework. Lives in main.rs (not a submodule) because it
// is intercepted at the very top of `main`, before ANYTHING else -- tracing
// init, the event loop, backend bring-up -- so `rubix screen off` run from a
// shell inside an already-running session talks to that session over IPC
// instead of booting a second compositor. `-c`/`--command` is unaffected: it
// stays exactly where it was, parsed after full bring-up (see below).

/// `rubix screen on|off|toggle|status [OUTPUT]`. `None` if `args` isn't a
/// `screen` invocation at all (caller falls through to normal startup);
/// `Some(exit_code)` otherwise, which the caller must `std::process::exit`
/// immediately -- this never returns into compositor bring-up.
fn handle_screen_subcommand(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("screen") {
        return None;
    }
    let rest = &args[1..];
    let Some(action) = rest.first().map(String::as_str) else {
        eprintln!("usage: rubix screen on|off|toggle|status [OUTPUT]");
        return Some(2);
    };
    let output = rest.get(1).cloned();

    let Some(socket_path) = resolve_rubix_socket() else {
        eprintln!("rubix screen: no rubix*.sock found in $XDG_RUNTIME_DIR -- is the compositor running?");
        return Some(1);
    };
    let mut stream = match std::os::unix::net::UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rubix screen: failed to connect to {}: {e}", socket_path.display());
            return Some(1);
        }
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));

    let code = match action {
        "on" | "off" => {
            let on = action == "on";
            if !send_screen_power(&mut stream, on, output.as_deref()) {
                1
            } else {
                println!("{} {}", if on { "on" } else { "off" }, output.as_deref().unwrap_or("(all outputs)"));
                0
            }
        }
        "toggle" => {
            let Some(status) = fetch_screen_status(&mut stream) else { return Some(1) };
            let on = match &output {
                Some(name) => match status.iter().find(|(n, _)| n == name) {
                    Some((_, currently_on)) => !*currently_on,
                    None => {
                        eprintln!("rubix screen: no such output: {name}");
                        return Some(1);
                    }
                },
                // Toggle-ALL converges rather than flips per-output: if
                // anything is still lit, turn everything off; only once
                // EVERY output is already off does it turn everything back
                // on. A plain per-output XOR has no well-defined "next
                // state" on a mixed multi-monitor setup (some on, some off).
                None => !status.iter().any(|(_, on)| *on),
            };
            if !send_screen_power(&mut stream, on, output.as_deref()) {
                1
            } else {
                println!("{}", if on { "on" } else { "off" });
                0
            }
        }
        "status" => {
            let Some(status) = fetch_screen_status(&mut stream) else { return Some(1) };
            let rows: Vec<_> = match &output {
                Some(name) => status.into_iter().filter(|(n, _)| n == name).collect(),
                None => status,
            };
            if let Some(name) = &output {
                if rows.is_empty() {
                    eprintln!("rubix screen: no such output: {name}");
                    return Some(1);
                }
            }
            for (name, on) in rows {
                println!("{name}\t{}", if on { "on" } else { "off" });
            }
            0
        }
        other => {
            eprintln!("rubix screen: unknown action '{other}' (expected on|off|toggle|status)");
            2
        }
    };
    Some(code)
}

/// Locate Rubix's IPC socket. Mirrors `contrib/waybar/rubixBar.py`'s
/// `socketPath()` exactly, so the CLI agrees with every other IPC client
/// about which socket a running session actually bound: prefer the
/// display-agnostic `rubix.sock`, else the most recently modified
/// `rubix-<n>.sock`.
fn resolve_rubix_socket() -> Option<std::path::PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let plain = std::path::PathBuf::from(&runtime_dir).join("rubix.sock");
    if plain.exists() {
        return Some(plain);
    }
    let mut best: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(&runtime_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("rubix-") || !name.ends_with(".sock") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        let is_newer = best.as_ref().map(|(_, t)| modified > *t).unwrap_or(true);
        if is_newer {
            best = Some((entry.path(), modified));
        }
    }
    best.map(|(p, _)| p)
}

/// Send `set_screen_power` and confirm the compositor answered `Ok` (not
/// `Error`, and not silence -- a closed connection or a timed-out read both
/// count as failure). `true` on success.
fn send_screen_power(stream: &mut std::os::unix::net::UnixStream, on: bool, output: Option<&str>) -> bool {
    use std::io::Write;
    let request = serde_json::json!({ "type": "set_screen_power", "on": on, "output": output });
    let mut line = request.to_string();
    line.push('\n');
    if let Err(e) = stream.write_all(line.as_bytes()) {
        eprintln!("rubix screen: write failed: {e}");
        return false;
    }
    match read_ipc_reply(stream) {
        Some(reply) if reply.get("type").and_then(|t| t.as_str()) == Some("error") => {
            let msg = reply.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
            eprintln!("rubix screen: compositor returned an error: {msg}");
            false
        }
        Some(_) => true,
        None => {
            eprintln!("rubix screen: no reply from compositor (timed out or connection closed)");
            false
        }
    }
}

/// Send `screen_status` and parse the `outputs` array back into `(name, on)`
/// pairs. `None` on any failure (already logged to stderr).
fn fetch_screen_status(stream: &mut std::os::unix::net::UnixStream) -> Option<Vec<(String, bool)>> {
    use std::io::Write;
    if let Err(e) = stream.write_all(b"{\"type\":\"screen_status\"}\n") {
        eprintln!("rubix screen: write failed: {e}");
        return None;
    }
    let reply = match read_ipc_reply(stream) {
        Some(r) => r,
        None => {
            eprintln!("rubix screen: no reply from compositor (timed out or connection closed)");
            return None;
        }
    };
    if reply.get("type").and_then(|t| t.as_str()) == Some("error") {
        let msg = reply.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
        eprintln!("rubix screen: compositor returned an error: {msg}");
        return None;
    }
    let Some(outputs) = reply.get("outputs").and_then(|o| o.as_array()) else {
        eprintln!("rubix screen: unexpected reply from compositor: {reply}");
        return None;
    };
    Some(
        outputs
            .iter()
            .filter_map(|o| {
                let name = o.get("name")?.as_str()?.to_string();
                let on = o.get("on")?.as_bool()?;
                Some((name, on))
            })
            .collect(),
    )
}

/// Read one newline-delimited JSON reply line off the socket.
fn read_ipc_reply(stream: &mut std::os::unix::net::UnixStream) -> Option<serde_json::Value> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if line.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&line).ok()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // MUST be the very first thing in `main`, before tracing init / event
    // loop / backend bring-up -- see the module doc above
    // `handle_screen_subcommand`. `-c`/`--command` (below, after full
    // bring-up) is untouched by this.
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = handle_screen_subcommand(&cli_args) {
        std::process::exit(code);
    }

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

    let mut event_loop: EventLoop<RubixState> = EventLoop::try_new()?;

    let display: Display<RubixState> = Display::new()?;
    let config = crate::config::Config::load();
    let mut data = RubixState::new(&mut event_loop, display, config);

    // Problems noticed while parsing the config above. Deliberately deferred
    // rather than reported inline: at this point neither sink can receive
    // anything. The notification daemon is usually itself in the `startup` list
    // (fired from the XWayland-ready hook below), and no IPC client has had a
    // chance to connect, let alone subscribe. Reporting now would mean the one
    // diagnostic a user most needs -- "your config did not parse" -- is the one
    // guaranteed to be dropped. A few seconds is enough for both sinks to exist.
    let mut startup_problems = crate::config::take_config_diagnostics();
    // Decoded here, before the backend starts, so the first frame drawn already
    // has a wallpaper rather than flashing black. Failures join the config
    // problems above and ride the same deferred report.
    startup_problems.extend(
        data.wallpaper
            .resolve(&data.config.wallpaper, &data.config.outputs),
    );
    // Slideshow plumbing: a channel for images decoded on worker threads, and
    // the timer that asks for the next swap. Both are inert when the wallpaper
    // is a single file.
    {
        use smithay::reexports::calloop::channel;
        let (tx, rx) = channel::channel();
        if event_loop
            .handle()
            .insert_source(rx, |event, _, data: &mut RubixState| {
                if let channel::Event::Msg(message) = event {
                    data.wallpaper.receive_prefetch(message);
                }
            })
            .is_ok()
        {
            data.wallpaper.set_decode_channel(tx);
        } else {
            // Without the channel the slideshow still runs; each swap just
            // decodes inline and hitches for a frame or two.
            tracing::warn!("wallpaper decode channel failed; slideshow will decode inline");
        }
    }
    data.rearm_wallpaper_timer();
    if !startup_problems.is_empty() {
        let mut pending = Some(startup_problems);
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            std::time::Duration::from_secs(5),
        );
        let _ = event_loop
            .handle()
            .insert_source(timer, move |_, _, data: &mut RubixState| {
                if let Some(problems) = pending.take() {
                    data.report_config_diagnostics(problems);
                }
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            });
    }

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

    init_xwayland(&event_loop, &data.display_handle);

    init_config_watch(&event_loop);

    crate::screencopy::init(&data.display_handle);

    crate::foreign_toplevel::init(&data.display_handle);

    let ipc_clients = crate::ipc::init_ipc(&event_loop, data.xdisplay);

    crate::portal::init_portal(&event_loop);

    // Optional startup command for nested dev (`rubix -c <cmd>`). No default
    // spawn: launchers (rofi via Super+space) and the config `startup` hook
    // cover normal session bring-up, so an empty desktop is the right default.
    let mut args = std::env::args().skip(1);
    let flag = args.next();
    let arg = args.next();

    if let (Some("-c") | Some("--command"), Some(command)) = (flag.as_deref(), arg) {
        std::process::Command::new(command).spawn().ok();
    }

    // After every dispatch cycle: refresh the space (enter/leave bookkeeping),
    // clean up dead popups, and -- critically -- flush queued events out to
    // clients. Without the flush, a client's opening registry roundtrip never
    // completes, so it blocks before ever creating a window (the winit backend
    // got away with flushing only in its Redraw handler; the udev render loop
    // doesn't, so this backend-neutral flush is what lets clients start at all).
    event_loop.run(None, &mut data, move |data| {
        data.space.refresh();
        data.popups.cleanup();
        let _ = data.display_handle.flush_clients();
        if std::mem::take(&mut data.ipc_dirty) {
            if let Some(clients) = &ipc_clients {
                crate::ipc::broadcast_snapshot(data, clients);
            }
            // Same signal, second audience: the status bar over IPC, window
            // lists (rofi, taskbars) over wlr-foreign-toplevel.
            crate::foreign_toplevel::refresh(data);
        }
        // Separate from the snapshot push above: config problems are discrete
        // events tied to one edit, not cube state, so they are neither coalesced
        // nor gated on `ipc_dirty`.
        if !data.pending_config_errors.is_empty() {
            let problems = std::mem::take(&mut data.pending_config_errors);
            if let Some(clients) = &ipc_clients {
                crate::ipc::broadcast_config_errors(clients, &problems);
            }
        }
    })?;

    Ok(())
}
