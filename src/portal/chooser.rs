//! Interactive source picker invoked from `Start`, in the same style as
//! `xdg-desktop-portal-wlr`/`-cosmic`: the backend owns the chooser UI, the
//! requesting client (frontend) shows nothing.
//!
//! Uses `rofi -dmenu`, not `slurp`: under Rubix's current layer-shell handling,
//! pointer clicks into layer-shell surfaces don't reliably register (slurp's
//! click-to-select failed in testing), while keyboard-driven layer-shell
//! clients -- rofi included -- work fine. The chooser is therefore entirely
//! keyboard-navigable.
//!
//! Spawned via `async-process` from the async zbus `Start` handler: no
//! compositor state is needed to build/run it (the label list is plain
//! `Send` data handed in by the caller), so this never touches the loop
//! thread or blocks the pw thread.

use async_process::{Command, Stdio};
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

use crate::portal::capture::CaptureTarget;
use crate::portal::screencast::{SourceInfo, SourceKind};

/// `SelectSources`/`AvailableSourceTypes` bitmask values, per
/// `org.freedesktop.portal.ScreenCast`.
pub const SOURCE_TYPE_MONITOR: u32 = 0b01;
pub const SOURCE_TYPE_WINDOW: u32 = 0b10;

/// Build the `(display_label, CaptureTarget)` list the chooser presents,
/// filtered to the source types the client actually requested in
/// `SelectSources` (`source_types`, the `SelectSources` `types` bitmask).
/// Order matches `sources`' own order (windows first, then monitors -- see
/// `screencast::build_source_list`).
pub fn build_choices(sources: &[SourceInfo], source_types: u32) -> Vec<(String, CaptureTarget)> {
    let mut choices = Vec::new();
    for source in sources {
        match source.kind {
            SourceKind::Monitor if source_types & SOURCE_TYPE_MONITOR != 0 => {
                choices.push((
                    format!("\u{1f5b5}  Monitor: {}", source.label),
                    CaptureTarget::Monitor(source.label.clone()),
                ));
            }
            SourceKind::Window if source_types & SOURCE_TYPE_WINDOW != 0 => {
                choices.push((format!("\u{1fa9f}  {}", source.label), CaptureTarget::Window(source.id)));
            }
            _ => {}
        }
    }
    choices
}

/// Run `rofi -dmenu -i -p "Share" -format i`, feed it `labels` (one per
/// line) on stdin, and parse the chosen row index back out of stdout.
///
/// Returns `None` on every cancel/failure path -- Escape, non-zero exit,
/// empty or unparseable stdout, or `rofi` missing from `PATH` -- so callers
/// must treat `None` uniformly as "no stream", never as an error to retry.
/// Never panics and never blocks the calling executor thread on the child
/// exiting (all I/O below is awaited, not synchronous).
pub async fn run_rofi(labels: &[String]) -> Option<usize> {
    if labels.is_empty() {
        tracing::warn!("[portal] chooser: no sources available for the requested source type(s)");
        return None;
    }

    let mut child = match Command::new("rofi")
        .args(["-dmenu", "-i", "-p", "Share", "-format", "i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            tracing::error!(
                "[portal] chooser: failed to spawn `rofi` (is it installed and on PATH?): {e}"
            );
            return None;
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        tracing::error!("[portal] chooser: rofi child has no stdin pipe");
        return None;
    };
    let input = labels.join("\n");
    if let Err(e) = stdin.write_all(input.as_bytes()).await {
        tracing::warn!("[portal] chooser: failed writing choices to rofi stdin: {e}");
    }
    // Close stdin so rofi's dmenu reader sees EOF and can display the list.
    drop(stdin);

    let mut stdout_buf = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        if let Err(e) = stdout.read_to_string(&mut stdout_buf).await {
            tracing::warn!("[portal] chooser: failed reading rofi stdout: {e}");
        }
    }

    let status = match child.status().await {
        Ok(status) => status,
        Err(e) => {
            tracing::warn!("[portal] chooser: failed waiting on rofi: {e}");
            return None;
        }
    };

    if !status.success() {
        // Escape / Ctrl+C in rofi's dmenu mode exits non-zero -- this is the
        // normal cancel path, not an error worth logging loudly.
        tracing::info!("[portal] chooser: rofi exited non-zero ({status:?}); treating as cancelled");
        return None;
    }

    match stdout_buf.trim().parse::<usize>() {
        Ok(idx) => Some(idx),
        Err(e) => {
            tracing::warn!(
                "[portal] chooser: rofi produced unparseable index ({stdout_buf:?}): {e}"
            );
            None
        }
    }
}

/// Full chooser flow: build labeled choices from `sources`/`source_types`,
/// run rofi, and map the returned index back to a [`CaptureTarget`]. `None`
/// on any cancel/failure path (see [`run_rofi`]) or an out-of-range index
/// (shouldn't happen with a well-behaved rofi, but never index out of bounds
/// on possibly-adversarial subprocess output).
pub async fn choose_target(sources: &[SourceInfo], source_types: u32) -> Option<CaptureTarget> {
    let choices = build_choices(sources, source_types);
    let labels: Vec<String> = choices.iter().map(|(label, _)| label.clone()).collect();
    let idx = run_rofi(&labels).await?;
    choices.into_iter().nth(idx).map(|(_, target)| target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sources() -> Vec<SourceInfo> {
        vec![
            SourceInfo { id: 7, kind: SourceKind::Window, label: "firefox".to_string() },
            SourceInfo { id: 0, kind: SourceKind::Monitor, label: "DP-1".to_string() },
            SourceInfo { id: 1, kind: SourceKind::Monitor, label: "HDMI-A-1".to_string() },
        ]
    }

    #[test]
    fn build_choices_filters_by_requested_type() {
        let sources = fake_sources();

        let windows_only = build_choices(&sources, SOURCE_TYPE_WINDOW);
        assert_eq!(windows_only.len(), 1);
        assert_eq!(windows_only[0].1, CaptureTarget::Window(7));

        let monitors_only = build_choices(&sources, SOURCE_TYPE_MONITOR);
        assert_eq!(monitors_only.len(), 2);
        assert_eq!(monitors_only[0].1, CaptureTarget::Monitor("DP-1".to_string()));
        assert_eq!(monitors_only[1].1, CaptureTarget::Monitor("HDMI-A-1".to_string()));

        let both = build_choices(&sources, SOURCE_TYPE_WINDOW | SOURCE_TYPE_MONITOR);
        assert_eq!(both.len(), 3);

        let neither = build_choices(&sources, 0);
        assert!(neither.is_empty());
    }

    #[test]
    fn build_choices_labels_are_readable() {
        let sources = fake_sources();
        let choices = build_choices(&sources, SOURCE_TYPE_WINDOW | SOURCE_TYPE_MONITOR);
        assert!(choices[0].0.contains("firefox"));
        assert!(choices[1].0.contains("Monitor: DP-1"));
    }

    /// Exercises the same index -> target mapping `choose_target` uses,
    /// without spawning rofi -- a stand-in for a fake selection index.
    #[test]
    fn index_maps_to_expected_target() {
        let sources = fake_sources();
        let choices = build_choices(&sources, SOURCE_TYPE_WINDOW | SOURCE_TYPE_MONITOR);
        let picked = choices.into_iter().nth(2).map(|(_, target)| target);
        assert_eq!(picked, Some(CaptureTarget::Monitor("HDMI-A-1".to_string())));
    }

    /// End-to-end sanity check of rofi's own `-format i` index output, run
    /// non-interactively via stdin/stdout redirection (`-normal-window` isn't
    /// needed since dmenu mode over pipes never opens a window when stdin is
    /// not a tty... but to be safe and avoid ever touching the user's live
    /// session, this test is opt-in only.
    #[test]
    #[ignore = "spawns a real rofi subprocess; run manually with `cargo test -- --ignored`"]
    fn rofi_format_i_parses_piped_index() {
        use std::io::Write;
        use std::process::{Command as StdCommand, Stdio as StdStdio};

        let mut child = StdCommand::new("rofi")
            .args(["-dmenu", "-i", "-p", "Share", "-format", "i", "-select", "1"])
            .stdin(StdStdio::piped())
            .stdout(StdStdio::piped())
            .spawn()
            .expect("rofi must be on PATH for this ignored test");
        child.stdin.take().unwrap().write_all(b"one\ntwo\nthree\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let idx: usize = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
        assert_eq!(idx, 1);
    }
}
