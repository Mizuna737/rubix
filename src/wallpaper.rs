//! Compositor-native wallpaper.
//!
//! ## Why this lives in the compositor
//!
//! An HDR wallpaper has to be *tagged* -- the compositor must be told the
//! pixels are ST 2084 (PQ) BT.2020 rather than sRGB, or it decodes them through
//! the wrong transfer function and the image comes out flat and grey. A Wayland
//! client does that with `wp_color_management_v1`. No wallpaper tool binds it:
//! `swaybg` has no notion of colour at all, and `mpvpaper` renders through
//! libmpv's render API into its own layer-shell surface, bypassing the mpv
//! Wayland VO where the tagging lives (verified: `set_image_description` is
//! never called). So there was nothing to adopt.
//!
//! Drawing it in-process skips the protocol round-trip entirely. The decoded
//! image already carries its transfer function in the file's CICP block, so the
//! [`DecodeKind`] is known at load time and simply handed to the same shader an
//! HDR client buffer would have taken. It also means the compositor owns the
//! pixels, which is what a future theming pass (palette extraction) needs.
//!
//! ## How it renders
//!
//! No new machinery. The wallpaper is a [`MemoryRenderBuffer`] -- already a
//! `RubixRenderElement` variant -- uploaded as `Xbgr2101010` for PQ content or
//! `Xbgr8888` for SDR. GLES3 accepts both (`SUPPORTED_MEM_FORMATS_3`), so a
//! 10-bit image needs no half-float conversion and costs 4 bytes per pixel.
//!
//! For a PQ wallpaper the element is wrapped in a [`RoundedElement`] at radius
//! zero, which is how a per-element shader program is installed (the mask is
//! inert at radius zero -- see rounding.rs). That routes it through
//! `DECODE_HDR_PQ` on an HDR output, or `tonemap_pq_to_sdr` on an SDR one, both
//! of which already exist. An SDR wallpaper is drawn unwrapped on an SDR output
//! and through the ordinary `DECODE_SDR` on an HDR one, exactly like any other
//! SDR surface.
//!
//! ## Animation
//!
//! Nothing here is animated yet, but the shape is built for it rather than
//! retrofitted. A [`Wallpaper`] holds a *list* of [`DecodedFrame`]s and a
//! cursor, and the render path asks it two questions -- [`Wallpaper::advance`]
//! ("did the frame change?") and [`Wallpaper::next_deadline`] ("when should I
//! repaint?"). A still image answers `false` and `None` forever, so the caller
//! is written once and an animated decoder is a change to this module alone.
//! `MemoryRenderBuffer::render` gives the in-place, damage-tracked upload path
//! a per-frame swap would need.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::reexports::calloop;
use smithay::backend::renderer::element::memory::{
    MemoryBuffer, MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer};
use smithay::utils::{Buffer as BufferCoords, Logical, Point, Rectangle, Scale, Size, Transform};

use crate::color_management::DecodeKind;
use crate::cursor::RubixRenderElement;
use crate::rounding::{GlesAccess, Refraction, RoundMode, RoundedElement, round_shaders};

/// How an image is mapped onto an output whose aspect ratio it does not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub enum WallpaperMode {
    /// Scale to cover the output, cropping the overflowing axis. The default:
    /// it is the only mode that both fills the screen and preserves aspect.
    #[default]
    Fill,
    /// Scale to fit inside the output, leaving bars on the shorter axis.
    Fit,
    /// Scale both axes to the output, ignoring aspect ratio.
    Stretch,
    /// No scaling. Centred, cropped if larger than the output.
    Center,
}

/// One decoded frame. `duration` is `None` for a still image; an animated
/// decoder fills it in and the render path starts calling [`Wallpaper::advance`].
pub(crate) struct DecodedFrame {
    pixels: Vec<u8>,
    duration: Option<Duration>,
}

/// The result of decoding a file: everything needed to build a [`Wallpaper`],
/// and nothing that needs a renderer. Kept separate from `Wallpaper` so the
/// format and colour decisions -- which are the parts that can be silently
/// wrong -- are testable without a GPU.
pub(crate) struct Decoded {
    frames: Vec<DecodedFrame>,
    size: Size<i32, BufferCoords>,
    fourcc: Fourcc,
    decode: DecodeKind,
    /// A pre-blurred copy of `frames[0].pixels`, same size and fourcc, built
    /// at decode time when backdrop frosting is enabled. `None` when frosting
    /// is off (`backdrop_blur_radius == 0` or no style enables it) -- see
    /// `blur_frame`.
    blurred: Option<Vec<u8>>,
}

/// A decoded image plus the GPU-side buffer it is uploaded through.
pub struct Wallpaper {
    frames: Vec<DecodedFrame>,
    current: usize,
    /// When the current frame was shown. Unused while `frames.len() == 1`.
    shown_at: Instant,
    buffer: MemoryRenderBuffer,
    /// The frosted variant of `buffer`, built once at decode time. `None`
    /// when frosting is off; a backdrop quad that wants blur but finds this
    /// `None` falls back to the sharp buffer rather than failing to draw.
    /// Never re-derived on `advance` -- see `blur_frame`'s doc comment for why
    /// that is fine for every wallpaper this compositor can currently decode.
    blurred: Option<MemoryRenderBuffer>,
    size: Size<i32, BufferCoords>,
    decode: DecodeKind,
}

impl Wallpaper {
    fn new(decoded: Decoded) -> Self {
        let Decoded { frames, size, fourcc, decode, blurred } = decoded;
        // Declared fully opaque: a wallpaper is the bottom of the stack and its
        // alpha is forced to 1.0 at decode. This is what lets occlusion culling
        // skip it entirely behind a fullscreen window, which matters because it
        // is the largest single element in the frame.
        let opaque = vec![Rectangle::from_size(size)];
        let buffer = MemoryRenderBuffer::from_memory(
            MemoryBuffer::from_slice(&frames[0].pixels, fourcc, size),
            1,
            Transform::Normal,
            Some(opaque.clone()),
        );
        let blurred = blurred.map(|pixels| {
            MemoryRenderBuffer::from_memory(
                MemoryBuffer::from_slice(&pixels, fourcc, size),
                1,
                Transform::Normal,
                Some(opaque),
            )
        });
        Wallpaper {
            frames,
            current: 0,
            shown_at: Instant::now(),
            buffer,
            blurred,
            size,
            decode,
        }
    }

    /// When the next frame is due, or `None` for a still image.
    ///
    /// The render loop uses this to arm a timer. A still wallpaper never arms
    /// one, so a static desktop stays at zero repaints.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let duration = self.frames.get(self.current)?.duration?;
        Some(self.shown_at + duration)
    }

    /// Advance to the frame due at `now`, uploading it if it changed.
    ///
    /// Returns whether the visible frame changed, so the caller knows whether
    /// to damage the output. Always `false` for a still image.
    pub(crate) fn advance(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.next_deadline() else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.current = (self.current + 1) % self.frames.len();
        self.shown_at = now;
        let mut context = self.buffer.render();
        context.draw(|dst| {
            dst.copy_from_slice(&self.frames[self.current].pixels);
            Result::<_, std::convert::Infallible>::Ok(vec![Rectangle::from_size(self.size)])
        })
        .expect("infallible");
        true
    }
}

/// A folder of images shown one at a time.
///
/// Distinct from the per-file animation seam above: that swaps *frames within
/// one image* and costs a memory copy, this swaps *whole files* and costs a
/// decode. A ~2.5K AVIF takes over 100ms to decode, which is a visible hitch at
/// any refresh rate, so a slideshow never decodes on the compositor thread --
/// see [`WallpaperManager::request_prefetch`].
struct Slideshow {
    /// Sorted, so the order is the same on every start. A directory listing is
    /// not ordered by anything on its own.
    entries: Vec<PathBuf>,
    index: usize,
    interval: Duration,
    /// When to move to the next entry.
    next_at: Instant,
}

impl Slideshow {
    fn peek_next(&self) -> &PathBuf {
        &self.entries[(self.index + 1) % self.entries.len()]
    }
}

/// A decoded image arriving from the prefetch thread.
pub struct PrefetchedWallpaper {
    path: PathBuf,
    /// The gain it was decoded at, so a result that raced a config change can
    /// be discarded rather than shown at the wrong brightness.
    scale: f32,
    /// The blur radius it was decoded at, for the same reason -- a radius
    /// change between request and delivery would deliver a wallpaper frosted
    /// at the wrong strength.
    blur_radius: u32,
    result: Result<Decoded, String>,
}

/// File extensions scanned when a directory is given. Matches what
/// [`decode_file`] can actually open -- a directory of mixed formats should
/// skip what it cannot read rather than fail the whole folder.
const SUPPORTED_EXTENSIONS: [&str; 4] = ["avif", "png", "jpg", "jpeg"];

/// Every readable image in a directory, sorted by path.
fn scan_directory(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .is_some_and(|e| SUPPORTED_EXTENSIONS.contains(&e.as_str()))
        })
        .collect();
    // Sorted by full path, which is unique -- no tiebreaker needed, and the
    // order is stable across restarts.
    entries.sort();
    if entries.is_empty() {
        return Err(format!(
            "no images found (looked for {})",
            SUPPORTED_EXTENSIONS.join(", "),
        ));
    }
    Ok(entries)
}

/// Per-output wallpaper assignment plus the decoded images backing it.
///
/// Images are keyed by path, not by output, so the common case -- one wallpaper
/// on every monitor -- decodes and uploads once. `MemoryRenderBuffer` is
/// internally shared, so handing the same one to two outputs costs nothing.
pub struct WallpaperManager {
    loaded: HashMap<PathBuf, Wallpaper>,
    assignments: HashMap<String, PathBuf>,
    /// The wallpaper for any output without an explicit assignment.
    default_path: Option<PathBuf>,
    mode: WallpaperMode,
    /// The gain the cached images were decoded at. Changing it invalidates
    /// every one of them, since the gain is baked into the pixels.
    scale: f32,
    /// Set when `path` names a directory rather than a file.
    slideshow: Option<Slideshow>,
    /// The next slideshow image, decoded ahead of time so the swap itself is
    /// free. `None` while a decode is in flight or none has been asked for.
    prefetch: Option<(PathBuf, Wallpaper)>,
    /// The path a worker thread is currently decoding, so the same image is not
    /// queued twice.
    inflight: Option<PathBuf>,
    /// What luminance in the *source image* counts as white when tone-mapping
    /// onto an SDR output -- measured before `scale` is applied, so the two are
    /// independent. See `config::WallpaperConfig::sdr_reference_nits`.
    sdr_reference_nits: f32,
    /// Ceiling in nits for a backdrop quad's highlights. Distinct from
    /// `sdr_reference_nits` on purpose -- that one normalises the whole
    /// wallpaper for an SDR output, this one caps what bleeds through a
    /// translucent window. See `config::DecorationConfig::backdrop_luminance_nits`.
    backdrop_luminance_nits: f32,
    /// The backdrop blur radius to decode at, already collapsed to `0` when no
    /// style enables `backdrop_blur` -- see `WallpaperManager::resolve`. Baked
    /// into the decoded pixels like `scale`, so a change invalidates the cache
    /// the same way.
    backdrop_blur_radius: u32,
    /// Handed to worker threads to send results back onto the event loop.
    /// `None` in tests and before `main` wires the channel up, in which case
    /// prefetching is simply skipped and swaps decode inline.
    decode_tx: Option<calloop::channel::Sender<PrefetchedWallpaper>>,
}

impl Default for WallpaperManager {
    fn default() -> Self {
        WallpaperManager {
            loaded: HashMap::new(),
            assignments: HashMap::new(),
            default_path: None,
            mode: WallpaperMode::default(),
            // Not `derive(Default)`: a gain of 0.0 would decode every wallpaper
            // to black. Unity is the only sane starting point.
            scale: 1.0,
            sdr_reference_nits: crate::hdr_shaders::SDR_WHITE_NITS,
            backdrop_luminance_nits: crate::hdr_shaders::SDR_WHITE_NITS,
            backdrop_blur_radius: 0,
            slideshow: None,
            prefetch: None,
            inflight: None,
            decode_tx: None,
        }
    }
}

impl WallpaperManager {
    /// Rebuild assignments from config, decoding anything new and dropping
    /// anything no longer referenced.
    ///
    /// Called at startup and on every config reload, so an edit to the config
    /// file swaps the wallpaper live. Returns the problems worth surfacing to
    /// the user (a path that does not exist, an image that will not decode) --
    /// the caller routes them through the configured diagnostics sink.
    pub(crate) fn resolve(
        &mut self,
        wallpaper: &crate::config::WallpaperConfig,
        outputs: &[crate::config::OutputConfig],
        decoration: &crate::config::DecorationConfig,
    ) -> Vec<String> {
        self.mode = wallpaper.mode;
        // Live: this only feeds a shader uniform, so unlike `luminance_scale`
        // it needs no re-decode.
        self.sdr_reference_nits = wallpaper.sdr_reference_nits;
        // Live for the same reason: a shader uniform only, no re-decode.
        self.backdrop_luminance_nits = decoration.backdrop_luminance_nits;
        // Baked into the decoded pixels, so a changed gain means re-decoding
        // rather than re-uploading. Cheap enough to do on a config save (one
        // table build plus one pass over the image) and it keeps the render
        // path free of a per-frame uniform it would otherwise have to carry
        // through both the composite and the z-run.
        if (self.scale - wallpaper.luminance_scale).abs() > 1e-4 {
            self.loaded.clear();
        }
        self.scale = wallpaper.luminance_scale;
        // Collapsed to 0 unless some style can actually turn frosting on --
        // skips the decode-time blur pass entirely on the overwhelmingly
        // common case where nobody has opted in, same idea as `scale` above:
        // baked into the pixels, so a change re-decodes.
        let blur_enabled = decoration.active.backdrop_blur
            || decoration.inactive.backdrop_blur
            || decoration.rules.iter().any(|r| {
                r.active.backdrop_blur == Some(true) || r.inactive.backdrop_blur == Some(true)
            });
        let backdrop_blur_radius = if blur_enabled { decoration.backdrop_blur_radius } else { 0 };
        if backdrop_blur_radius != self.backdrop_blur_radius {
            self.loaded.clear();
            self.prefetch = None;
        }
        self.backdrop_blur_radius = backdrop_blur_radius;
        let mut problems = Vec::new();
        let mut wanted: Vec<PathBuf> = Vec::new();

        // A directory becomes a slideshow; a file is shown as-is. Reusing
        // `path` for both means there is no way to configure a folder and a
        // file at once and wonder which wins.
        // Remembered across the rebuild below so that editing an unrelated
        // setting -- `luminance_scale`, most often, since that is tuned by
        // repeated saves -- does not throw the rotation back to its first
        // image every time.
        let showing = self.slideshow.as_ref().map(|s| s.entries[s.index].clone());
        self.slideshow = None;
        self.prefetch = None;
        match &wallpaper.path {
            Some(path) if path.is_dir() => match scan_directory(path) {
                Ok(entries) => {
                    let interval = Duration::from_secs(wallpaper.interval_seconds.max(1));
                    // Rescanned every time, so images added to the folder are
                    // picked up; the current one is then found again by path
                    // rather than by its old index, which the rescan may have
                    // shifted.
                    let index = showing
                        .and_then(|current| entries.iter().position(|p| *p == current))
                        .unwrap_or(0);
                    tracing::info!(
                        "wallpaper slideshow {}: {} images every {}s (showing {})",
                        path.display(),
                        entries.len(),
                        wallpaper.interval_seconds,
                        entries[index].display(),
                    );
                    self.default_path = Some(entries[index].clone());
                    self.slideshow = Some(Slideshow {
                        entries,
                        index,
                        interval,
                        next_at: Instant::now() + interval,
                    });
                }
                Err(e) => problems.push(format!("wallpaper {}: {e}", path.display())),
            },
            other => self.default_path = other.clone(),
        }
        if let Some(path) = &self.default_path {
            wanted.push(path.clone());
        }
        self.assignments.clear();
        for output in outputs {
            if let Some(path) = &output.wallpaper {
                self.assignments.insert(output.name.clone(), path.clone());
                wanted.push(path.clone());
            }
        }

        for path in &wanted {
            if self.loaded.contains_key(path) {
                continue;
            }
            match load(path, self.scale, self.backdrop_blur_radius) {
                Ok(wallpaper) => {
                    tracing::info!(
                        "wallpaper {}: {}x{} {:?}",
                        path.display(),
                        wallpaper.size.w,
                        wallpaper.size.h,
                        wallpaper.decode,
                    );
                    self.loaded.insert(path.clone(), wallpaper);
                }
                Err(e) => problems.push(format!("wallpaper {}: {e}", path.display())),
            }
        }
        // Evict anything no longer referenced. A 4K 10-bit image is ~33 MB of
        // CPU memory plus the same again on the GPU, so this is worth doing
        // eagerly rather than letting an edited config accumulate old images.
        self.loaded.retain(|path, _| wanted.contains(path));
        self.prime_prefetch();
        problems
    }

    /// Point one output (or every output, with `output: None`) at a new image.
    ///
    /// This is the runtime path -- what the IPC `set_wallpaper` command calls.
    /// Decoding happens here rather than at draw time so a bad path fails
    /// with a message the caller can return, instead of silently drawing
    /// nothing on the next frame.
    pub(crate) fn set(&mut self, output: Option<&str>, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();
        if !self.loaded.contains_key(&path) {
            let wallpaper =
                load(&path, self.scale, self.backdrop_blur_radius)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            self.loaded.insert(path.clone(), wallpaper);
        }
        match output {
            Some(name) => {
                self.assignments.insert(name.to_string(), path);
            }
            None => {
                // A blanket set replaces the per-output overrides too --
                // otherwise "set the wallpaper" would visibly miss a monitor.
                self.assignments.clear();
                self.default_path = Some(path);
            }
        }
        self.evict_unreferenced();
        Ok(())
    }

    /// Give the manager a channel so prefetch decodes can run off-thread.
    /// Without one it still works, just synchronously -- which is what the
    /// tests use.
    pub(crate) fn set_decode_channel(
        &mut self,
        tx: calloop::channel::Sender<PrefetchedWallpaper>,
    ) {
        self.decode_tx = Some(tx);
        self.prime_prefetch();
    }

    /// When the current slideshow image should be replaced, or `None` if there
    /// is no slideshow. The event loop arms a timer on this.
    pub(crate) fn next_slideshow_at(&self) -> Option<Instant> {
        Some(self.slideshow.as_ref()?.next_at)
    }

    /// Start decoding the next slideshow image if nothing is queued already.
    ///
    /// Runs on a worker thread: a ~2.5K AVIF costs over 100ms, and the whole
    /// point of prefetching is that the compositor never pays it. With no
    /// channel wired up (tests) this is a no-op and the swap decodes inline
    /// instead -- correct either way, just not smooth.
    fn prime_prefetch(&mut self) {
        let Some(slideshow) = &self.slideshow else { return };
        if slideshow.entries.len() < 2 {
            return;
        }
        let next = slideshow.peek_next().clone();
        if self.inflight.as_ref() == Some(&next)
            || self.prefetch.as_ref().is_some_and(|(path, _)| path == &next)
        {
            return;
        }
        let Some(tx) = self.decode_tx.clone() else { return };
        let scale = self.scale;
        let blur_radius = self.backdrop_blur_radius;
        self.inflight = Some(next.clone());
        // A detached thread per image rather than a pool: one decode every
        // `interval` seconds, and the thread exits as soon as it has sent.
        std::thread::spawn(move || {
            let result = decode_file(&next, scale, blur_radius);
            // A send failure means the compositor is gone; there is nothing
            // useful to do about it and nothing to clean up.
            let _ = tx.send(PrefetchedWallpaper { path: next, scale, blur_radius, result });
        });
    }

    /// Take delivery of a prefetched image from the worker thread.
    pub(crate) fn receive_prefetch(&mut self, message: PrefetchedWallpaper) {
        if self.inflight.as_ref() == Some(&message.path) {
            self.inflight = None;
        }
        // A gain or blur-radius change between request and delivery would show
        // this image at the wrong brightness or frost strength; drop it and
        // let `prime_prefetch` reissue.
        if (message.scale - self.scale).abs() > 1e-4
            || message.blur_radius != self.backdrop_blur_radius
        {
            self.prime_prefetch();
            return;
        }
        match message.result {
            Ok(decoded) => {
                self.prefetch = Some((message.path, Wallpaper::new(decoded)));
            }
            Err(e) => {
                // Logged, not surfaced through the diagnostics sink: one
                // unreadable file in a folder of a hundred is not worth a
                // notification, and the slideshow simply steps over it.
                tracing::warn!("wallpaper {}: {e}", message.path.display());
                self.skip_unreadable(&message.path);
            }
        }
    }

    /// Drop an entry that would not decode, so the slideshow stops trying.
    fn skip_unreadable(&mut self, path: &Path) {
        let Some(slideshow) = &mut self.slideshow else { return };
        if slideshow.entries.len() < 2 {
            return;
        }
        if let Some(position) = slideshow.entries.iter().position(|p| p == path) {
            slideshow.entries.remove(position);
            if position <= slideshow.index && slideshow.index > 0 {
                slideshow.index -= 1;
            }
            // Guard against the index falling off the end after a removal.
            slideshow.index %= slideshow.entries.len();
        }
        self.prime_prefetch();
    }

    /// Move to the next slideshow image if it is due. Returns whether the
    /// visible wallpaper changed.
    ///
    /// If the prefetch has not landed yet the swap is *skipped*, not blocked:
    /// the current image stays up and the next tick tries again. A slideshow
    /// that stutters by a few hundred milliseconds is invisible; one that
    /// blocks the compositor for that long is not.
    pub(crate) fn advance_slideshow(&mut self, now: Instant) -> bool {
        let Some(slideshow) = &self.slideshow else { return false };
        if now < slideshow.next_at {
            return false;
        }
        let wanted = slideshow.peek_next().clone();
        let Some((path, wallpaper)) = self.prefetch.take().filter(|(p, _)| p == &wanted) else {
            // Not ready. Re-arm for a short retry rather than pushing the whole
            // schedule out by a full interval.
            if let Some(slideshow) = &mut self.slideshow {
                slideshow.next_at = now + Duration::from_millis(250);
            }
            self.prime_prefetch();
            return false;
        };
        let Some(slideshow) = &mut self.slideshow else { return false };
        slideshow.index = (slideshow.index + 1) % slideshow.entries.len();
        slideshow.next_at = now + slideshow.interval;
        self.loaded.insert(path.clone(), wallpaper);
        self.default_path = Some(path);
        self.evict_unreferenced();
        self.prime_prefetch();
        true
    }

    fn evict_unreferenced(&mut self) {
        let default = self.default_path.clone();
        self.loaded.retain(|path, _| {
            Some(path) == default.as_ref() || self.assignments.values().any(|p| p == path)
        });
    }

    fn path_for(&self, output: &str) -> Option<&PathBuf> {
        self.assignments.get(output).or(self.default_path.as_ref())
    }

    pub(crate) fn for_output(&self, output: &str) -> Option<&Wallpaper> {
        self.loaded.get(self.path_for(output)?)
    }

    /// Advance every output's wallpaper and report whether any frame changed.
    /// Inert while all wallpapers are stills.
    // Not `any`, despite what clippy suggests: `any` short-circuits on the
    // first wallpaper that changed, leaving every other output stuck on a stale
    // frame. Every wallpaper must be advanced, and the return value is a
    // summary of that -- not a search.
    #[allow(clippy::unnecessary_fold)]
    pub(crate) fn advance_all(&mut self, now: Instant) -> bool {
        self.loaded
            .values_mut()
            .fold(false, |changed, w| w.advance(now) || changed)
    }

    /// Whether any output is showing HDR-range wallpaper content. Drives the
    /// same connector/tone-map decisions an HDR *window* drives.
    pub(crate) fn output_has_hdr(&self, output: &str) -> bool {
        self.for_output(output).is_some_and(|w| w.decode.is_hdr())
    }

    /// Build the render element for one output, paired with its
    /// [`DecodeKind`], or `None` if the output has no wallpaper.
    /// `output_size` is the output's logical size.
    ///
    /// `tonemap` says the destination is 8-bit sRGB with no linear working
    /// space -- an SDR output showing an HDR image, or a capture. Only that
    /// case gets a per-element program; every other path already installs the
    /// right one for the whole pass (an HDR output's composite decode, or the
    /// z-run's per-run decode), and wrapping there would fight it for the one
    /// program slot. The returned `DecodeKind` is what those callers tag with.
    ///
    /// The tone-map uses the wallpaper's own `sdr_reference_nits`, not the
    /// compositor's live `sdr_white_nits`. The two paths measure against
    /// different things: on an HDR output the image is shown at its absolute
    /// graded luminance, while tone-mapping normalises it against a reference
    /// white. Content graded well above that reference -- SDR-to-HDR
    /// conversions routinely lift the whole image -- therefore looks correct on
    /// the HDR output and far too bright on the SDR one, with the shadows
    /// lifted worst. One number cannot serve both, so this one is separate.
    pub(crate) fn element<R>(
        &self,
        renderer: &mut R,
        output: &str,
        output_size: Size<i32, Logical>,
        scale: f64,
        tonemap: bool,
    ) -> Option<(DecodeKind, RubixRenderElement<R>)>
    where
        R: Renderer + ImportAll + ImportMem + GlesAccess,
        R::TextureId: Send + Clone + 'static,
    {
        let wallpaper = self.for_output(output)?;
        let placement = place(self.mode, wallpaper.size, output_size);
        let element = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            placement.loc.to_f64().to_physical(scale),
            &wallpaper.buffer,
            None,
            Some(placement.src),
            Some(placement.size),
            Kind::Unspecified,
        )
        .map_err(|e| tracing::warn!("wallpaper import failed on {output}: {e:?}"))
        .ok()?;

        let kind = wallpaper.decode;
        if !tonemap || !kind.is_hdr() {
            return Some((kind, RubixRenderElement::Memory(element)));
        }
        let Some(shaders) = round_shaders(renderer.gles_renderer()) else {
            // Shaders would not compile. Drawing PQ texels through the sRGB
            // program is wrong, but a blank desktop is worse, and
            // `round_shaders` has already logged it once.
            return Some((kind, RubixRenderElement::Memory(element)));
        };
        Some((
            kind,
            RubixRenderElement::RoundedMemory(RoundedElement::new(
                element,
                &shaders,
                RoundMode::Tonemap(kind),
                // Radius zero: the wrapper is here for the program, not the
                // mask, so `window_rect` is unused but must be well-defined.
                0.0,
                Rectangle::default(),
                Scale::from(scale),
                // Scaled by the decode gain so the two knobs stay orthogonal.
                // `luminance_scale` is already baked into the texels, so a
                // fixed reference here would mean every HDR-brightness tweak
                // silently re-broke the SDR output in the opposite direction.
                // Multiplying it through cancels: (nits * gain) / (ref * gain).
                self.sdr_reference_nits * self.scale,
                Refraction::NONE,
            )),
        ))
    }

    /// A per-window backdrop quad: the wallpaper element again, but
    /// source-cropped to `window_rect` and, when `blur` is set, sampling the
    /// pre-blurred buffer instead of the sharp one.
    ///
    /// Reuses the exact tone-map wrapper `element` uses above rather than a
    /// new path -- same `RoundMode::Tonemap` program that already renders the
    /// HDR wallpaper correctly on a non-HDR output, just aimed at a cropped
    /// source rect instead of the whole image.
    ///
    /// `window_rect` is in the same region-local logical space as `element`'s
    /// `output_size` -- i.e. relative to this output's own origin, not global
    /// compositor space.
    ///
    /// `hdr_pass` picks which wrapper program a tone-mapped quad gets:
    /// `false` (an SDR output, or an SDR-showing-HDR-content output, or a
    /// capture) wraps with `RoundMode::Tonemap`, which decodes and rolls off
    /// straight to sRGB 0..1. `true` (a per-window backdrop on the **HDR
    /// composite pass** -- see `rounding::space_elements` with
    /// `SpaceMode::HdrComposite`) wraps with
    /// `RoundMode::TonemapAbs10k` instead, which stays in the abs10k working
    /// space that pass's destination actually is. Writing the sRGB program's
    /// 0..1 output into that offscreen would read a 0.77 pixel back as 7700
    /// cd/m^2 -- a blown-out backdrop, not a capped one.
    ///
    /// Returns the wallpaper's own `DecodeKind` alongside the element, same
    /// shape as `element` above. The kind used to be how the HDR pass tagged
    /// this quad for its z-run partition; that partition is gone and the quad
    /// now carries its own program, so the kind is informational.
    ///
    /// `sdr_white_nits` is the pass's real SDR white point and
    /// `self.backdrop_luminance_nits` is the tone curve's reference ceiling.
    /// Both ride the same `sdr_white_nits` uniform name, so which one is
    /// passed depends on the mode -- see the match below.
    // `tonemap` and `blur` stay two separate flags rather than one bundled
    // struct: they are deliberately orthogonal knobs (a capped-but-sharp
    // backdrop is a real look), and collapsing them to satisfy the arity lint
    // would hide that from the signature.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn backdrop_element<R>(
        &self,
        renderer: &mut R,
        output: &str,
        output_size: Size<i32, Logical>,
        window_rect: Rectangle<i32, Logical>,
        scale: f64,
        tonemap: bool,
        blur: bool,
        hdr_pass: bool,
        sdr_white_nits: f32,
        refraction: Refraction,
    ) -> Option<(DecodeKind, RubixRenderElement<R>)>
    where
        R: Renderer + ImportAll + ImportMem + GlesAccess,
        R::TextureId: Send + Clone + 'static,
    {
        let wallpaper = self.for_output(output)?;
        let placement = place(self.mode, wallpaper.size, output_size);
        let crop = crop_for_window(&placement, window_rect);
        // Only this function knows both the crop (the wallpaper's own space)
        // and the buffer size, so `uv_per_px` is computed here rather than by
        // the caller -- the caller owns the look (strength, facet size,
        // dispersion), this owns the geometry. Guarded with `.max(1.0)`
        // because a zero-size window rect is reachable mid-resize and must not
        // produce a NaN offset that blanks the quad.
        let buffer_w = (wallpaper.size.w as f64).max(1.0);
        let buffer_h = (wallpaper.size.h as f64).max(1.0);
        let dst_physical = window_rect.size.to_f64().to_physical(scale);
        let dst_w = dst_physical.w.max(1.0);
        let dst_h = dst_physical.h.max(1.0);
        let (uv_origin, uv_per_px) =
            refraction_mapping(crop, (buffer_w, buffer_h), (dst_w, dst_h));
        let refraction = Refraction { uv_origin, uv_per_px, ..refraction };
        let refract = refraction.strength > 0.0;

        // Falls back to the sharp buffer if there is no blurred one -- either
        // frosting is off globally, or this particular wallpaper's fourcc
        // isn't one `blur_frame` covers. Better a sharp backdrop than none.
        let buffer = if blur {
            wallpaper.blurred.as_ref().unwrap_or(&wallpaper.buffer)
        } else {
            &wallpaper.buffer
        };

        let element = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            window_rect.loc.to_f64().to_physical(scale),
            buffer,
            None,
            Some(crop),
            Some(window_rect.size),
            Kind::Unspecified,
        )
        .map_err(|e| tracing::warn!("backdrop import failed on {output}: {e:?}"))
        .ok()?;

        let kind = wallpaper.decode;
        // Which program this quad must be drawn with, and which value the
        // overloaded `sdr_white_nits` uniform carries for it.
        //
        // `None` means the quad needs no program of its own: the destination
        // is SDR and the pass-wide default is already correct for it.
        //
        // The `(false, true)` arm is the one that is easy to miss. On the HDR
        // composite pass there is no pass-wide default at all -- every element
        // installs its own program -- so a quad returned bare would draw with
        // whatever the previous element happened to leave in the slot. It
        // still needs a plain decode even when nothing is being tone-mapped,
        // and that decode wants the *real* SDR white point, not the backdrop
        // ceiling. Reachable two ways: `backdrop_blur` on with
        // `backdrop_tonemap` off, and any SDR wallpaper on an HDR output.
        let Some(mode) = backdrop_program(tonemap, kind, hdr_pass, refract) else {
            return Some((kind, RubixRenderElement::Memory(element)));
        };
        // A plain decode's uniform is the real SDR white point; the two
        // tone-mapping modes overload the same name as the curve's ceiling.
        let reference_nits = match mode {
            RoundMode::Decode(_) => sdr_white_nits,
            _ => self.backdrop_luminance_nits,
        };
        // Shaders failed to compile. On an SDR pass this is cosmetic -- an
        // uncapped backdrop. On the HDR pass it means this quad inherits the
        // previous element's program, which is wrong but is also the same
        // degradation every other element in the pass suffers from the same
        // failure; there is nothing better available at this point.
        let Some(shaders) = round_shaders(renderer.gles_renderer()) else {
            return Some((kind, RubixRenderElement::Memory(element)));
        };
        Some((
            kind,
            RubixRenderElement::RoundedMemory(RoundedElement::new(
                element,
                &shaders,
                mode,
                0.0,
                Rectangle::default(),
                Scale::from(scale),
                // NOT scaled by the decode gain, unlike `element`'s
                // `sdr_reference_nits`. The two knobs live in different
                // reference frames: `sdr_reference_nits` is measured against
                // the file's own grading (pre-gain), so it has to carry the
                // gain to compare against post-gain texels. This one is an
                // absolute display-referred ceiling -- the luminance a
                // backdrop may actually reach on screen, which is what makes
                // it comparable to `sdr_white_nits`. The gain is already baked
                // into the texels, so multiplying here would scale the ceiling
                // a second time (40 nits became 8, putting the knee at the
                // wallpaper's median and flattening half the image).
                reference_nits,
                refraction,
            )),
        ))
    }
}

/// Which program a backdrop quad must be drawn with, or `None` if it needs
/// none of its own.
///
/// Split out as a pure function because getting it wrong is invisible
/// everywhere except on a screen: the wrong arm still builds, still logs
/// clean, and still draws something.
/// The `(uv_origin, uv_per_px)` pair that lets the refraction shader undo a
/// backdrop quad's crop and recover window-local pixels.
///
/// This is the whole of the window-versus-desktop anchoring question, which is
/// why it is a named function with a test rather than four lines inline. A
/// backdrop quad samples a *slice* of the shared wallpaper, so its `v_coords`
/// neither start at zero nor span the full texture, and both facts change as
/// the window moves. Feeding raw `v_coords` to the facet field anchored the
/// crystal to the wallpaper: dragging a window scooped up different facets the
/// whole way across, instead of carrying its own.
///
/// The invariant the test pins is a round trip -- `(v_coords - uv_origin) /
/// uv_per_px` must be `0` at the crop's top-left and the quad's size in
/// physical pixels at its bottom-right.
pub(crate) fn refraction_mapping(
    crop: Rectangle<f64, Logical>,
    buffer: (f64, f64),
    dst_physical: (f64, f64),
) -> ((f32, f32), (f32, f32)) {
    let (buffer_w, buffer_h) = buffer;
    let (dst_w, dst_h) = dst_physical;
    (
        ((crop.loc.x / buffer_w) as f32, (crop.loc.y / buffer_h) as f32),
        (
            ((crop.size.w / buffer_w) / dst_w) as f32,
            ((crop.size.h / buffer_h) / dst_h) as f32,
        ),
    )
}

pub(crate) fn backdrop_program(
    tonemap: bool,
    kind: DecodeKind,
    hdr_pass: bool,
    refract: bool,
) -> Option<RoundMode> {
    match (tonemap && kind.is_hdr(), hdr_pass) {
        // Cap the backdrop's luminance without leaving the abs10k working
        // space the HDR composite pass draws into.
        (true, true) => Some(RoundMode::TonemapAbs10k(kind)),
        // Same rolloff, but the destination is 8-bit sRGB, so collapse to it.
        (true, false) => Some(RoundMode::Tonemap(kind)),
        // Nothing to tone-map, but the HDR pass has no pass-wide default to
        // inherit -- every element installs its own program, so a quad with
        // none would draw with whatever the previous element left behind.
        (false, true) => Some(RoundMode::Decode(kind)),
        // A refracting quad must carry a program even when the colour path
        // wants none, because the refraction lives in the fragment shader. The
        // plain program is the right one: it is what this quad would have been
        // drawn with anyway, now with the sample site rewritten.
        (false, false) => refract.then_some(RoundMode::Plain),
    }
}

/// The crop of a placed wallpaper's source rect that lands under `window_rect`
/// (region-local logical, same space as `Placement::loc`/`size`).
///
/// `placement.src.size / placement.size` is buffer pixels per output pixel --
/// the same ratio the whole image was scaled by -- so offsetting and scaling
/// `window_rect` by it, relative to `placement.loc`, gives exactly the source
/// rect that image content occupies under the window. Pure geometry, so it is
/// tested the same way `place` is, without a renderer.
pub(crate) fn crop_for_window(
    placement: &Placement,
    window_rect: Rectangle<i32, Logical>,
) -> Rectangle<f64, Logical> {
    let sx = placement.src.size.w / (placement.size.w.max(1) as f64);
    let sy = placement.src.size.h / (placement.size.h.max(1) as f64);
    Rectangle::new(
        Point::from((
            placement.src.loc.x + (window_rect.loc.x - placement.loc.x) as f64 * sx,
            placement.src.loc.y + (window_rect.loc.y - placement.loc.y) as f64 * sy,
        )),
        Size::from((window_rect.size.w as f64 * sx, window_rect.size.h as f64 * sy)),
    )
}

/// Where a decoded image lands on an output, in the terms
/// `MemoryRenderBufferRenderElement::from_buffer` wants.
#[derive(Debug, PartialEq)]
pub(crate) struct Placement {
    /// Crop rectangle in *buffer* pixels.
    pub src: Rectangle<f64, Logical>,
    /// Destination size in logical coordinates.
    pub size: Size<i32, Logical>,
    /// Destination offset within the output, in logical coordinates.
    pub loc: Point<i32, Logical>,
}

/// Pure geometry: how an image of size `buffer` maps onto an output of size
/// `output` under `mode`. Separated out so the aspect-ratio arithmetic is
/// testable without a renderer.
pub(crate) fn place(
    mode: WallpaperMode,
    buffer: Size<i32, BufferCoords>,
    output: Size<i32, Logical>,
) -> Placement {
    let (bw, bh) = (buffer.w.max(1) as f64, buffer.h.max(1) as f64);
    let (ow, oh) = (output.w.max(1) as f64, output.h.max(1) as f64);
    let full = Rectangle::from_size(Size::from((bw, bh)));

    match mode {
        WallpaperMode::Stretch => Placement {
            src: full,
            size: output,
            loc: Point::default(),
        },
        WallpaperMode::Fill => {
            // Cover: scale by the larger ratio, then crop the axis that
            // overflows back to what the output can show.
            let scale = (ow / bw).max(oh / bh);
            let crop = Size::from((ow / scale, oh / scale));
            Placement {
                src: Rectangle::new(
                    Point::from(((bw - crop.w) / 2.0, (bh - crop.h) / 2.0)),
                    crop,
                ),
                size: output,
                loc: Point::default(),
            }
        }
        WallpaperMode::Fit => {
            // Contain: scale by the smaller ratio and centre the result. The
            // uncovered margin is whatever was cleared before the wallpaper
            // drew -- black.
            let scale = (ow / bw).min(oh / bh);
            let size = Size::from(((bw * scale).round() as i32, (bh * scale).round() as i32));
            Placement {
                src: full,
                size,
                loc: Point::from(((output.w - size.w) / 2, (output.h - size.h) / 2)),
            }
        }
        WallpaperMode::Center => {
            // 1:1. Crop whichever axes are too large, centre whichever are too
            // small; an image bigger on one axis and smaller on the other does
            // both at once, which is why each axis is computed independently.
            let crop = Size::from((bw.min(ow), bh.min(oh)));
            let size = Size::from((crop.w as i32, crop.h as i32));
            Placement {
                src: Rectangle::new(
                    Point::from(((bw - crop.w) / 2.0, (bh - crop.h) / 2.0)),
                    crop,
                ),
                size,
                loc: Point::from(((output.w - size.w) / 2, (output.h - size.h) / 2)),
            }
        }
    }
}

// SMPTE ST 2084 (PQ) constants. Same numbers as the shader's `pq_eotf`; this
// is the CPU-side pair, used to apply a luminance gain at decode time.
const PQ_M1: f64 = 0.1593017578125;
const PQ_M2: f64 = 78.84375;
const PQ_C1: f64 = 0.8359375;
const PQ_C2: f64 = 18.8515625;
const PQ_C3: f64 = 18.6875;

/// PQ signal (0..1) -> luminance in cd/m². Inverse of [`pq_oetf`].
fn pq_eotf(signal: f64) -> f64 {
    let ep = signal.max(0.0).powf(1.0 / PQ_M2);
    let num = (ep - PQ_C1).max(0.0);
    let den = PQ_C2 - PQ_C3 * ep;
    if den <= 0.0 {
        return 10000.0;
    }
    (num / den).powf(1.0 / PQ_M1) * 10000.0
}

/// Luminance in cd/m² -> PQ signal (0..1). Inverse of [`pq_eotf`].
fn pq_oetf(nits: f64) -> f64 {
    let y = (nits / 10000.0).clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
}

/// A 10-bit PQ code -> 10-bit PQ code table applying a linear-light gain.
///
/// Done as a lookup rather than per-pixel maths because the gain lives in
/// linear light while the pixels are PQ-encoded: each sample would otherwise
/// need two `powf` pairs, and a 4K image has 25 million of them. There are only
/// 1024 possible inputs, so the whole transform collapses to one table built in
/// microseconds and applied with an array index.
fn pq_gain_table(scale: f32) -> [u16; 1024] {
    let scale = scale as f64;
    let mut table = [0u16; 1024];
    for (code, entry) in table.iter_mut().enumerate() {
        let nits = pq_eotf(code as f64 / 1023.0) * scale;
        *entry = (pq_oetf(nits) * 1023.0).round().clamp(0.0, 1023.0) as u16;
    }
    table
}

/// The sRGB counterpart, for an SDR wallpaper. Same idea, 256 entries.
fn srgb_gain_table(scale: f32) -> [u8; 256] {
    fn to_linear(v: f64) -> f64 {
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    }
    fn to_srgb(v: f64) -> f64 {
        if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
    }
    let scale = scale as f64;
    let mut table = [0u8; 256];
    for (code, entry) in table.iter_mut().enumerate() {
        let linear = (to_linear(code as f64 / 255.0) * scale).clamp(0.0, 1.0);
        *entry = (to_srgb(linear) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    table
}

/// Whether a gain is close enough to 1.0 to skip building a table for it.
fn is_unity(scale: f32) -> bool {
    (scale - 1.0).abs() < 1e-4
}

// ---- backdrop frosting (decoration `backdrop_blur`) ----
//
// A separable box blur, run three times, approximates a Gaussian closely
// enough for a frosted-glass look and costs O(w*h) per pass *regardless of
// radius* via a running sum -- no kernel-size blow-up the way a direct
// convolution would have. Dual-Kawase (mip-chain up/downsample) would be
// cheaper still, but that algorithm exists to make blur affordable *every
// frame*; this runs once per decode, so the extra complexity buys nothing
// here.
//
// It runs in linear light, not on the encoded (PQ or sRGB) codes directly:
// blur models optical scatter, which is a property of physical luminance, not
// of however that luminance happens to be quantised for storage. Blurring the
// codes would smear perceptual steps instead of light.

/// One line (row or column) of a box blur via a running sum, edges extended
/// by clamping the window to the line's bounds (mirrors what a convolution
/// with a clamp-to-edge sampler would do). `radius == 0` is the identity.
fn box_blur_line(src: &[f32], dst: &mut [f32], radius: usize) {
    let n = src.len();
    if n == 0 {
        return;
    }
    if radius == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let clamp = |i: isize| -> usize { i.clamp(0, n as isize - 1) as usize };
    let ir = radius as isize;
    let mut sum = 0.0f32;
    for k in -ir..=ir {
        sum += src[clamp(k)];
    }
    let window = (2 * radius + 1) as f32;
    dst[0] = sum / window;
    for (x, out) in dst.iter_mut().enumerate().skip(1) {
        let add = clamp(x as isize + ir);
        let sub = clamp(x as isize - ir - 1);
        sum += src[add] - src[sub];
        *out = sum / window;
    }
}

fn box_blur_horizontal(src: &[f32], dst: &mut [f32], w: usize, h: usize, radius: usize) {
    for y in 0..h {
        box_blur_line(&src[y * w..(y + 1) * w], &mut dst[y * w..(y + 1) * w], radius);
    }
}

/// Same as the horizontal pass, transposed. A scratch column pair is reused
/// across every column rather than allocated per-column, since `w` can be in
/// the thousands.
fn box_blur_vertical(src: &[f32], dst: &mut [f32], w: usize, h: usize, radius: usize) {
    let mut column = vec![0.0f32; h];
    let mut blurred = vec![0.0f32; h];
    for x in 0..w {
        for y in 0..h {
            column[y] = src[y * w + x];
        }
        box_blur_line(&column, &mut blurred, radius);
        for y in 0..h {
            dst[y * w + x] = blurred[y];
        }
    }
}

/// Three passes of horizontal-then-vertical box blur, in place. `radius == 0`
/// is a no-op (each line pass is the identity, so three of them still are).
fn box_blur_plane(plane: &mut [f32], w: usize, h: usize, radius: usize) {
    if radius == 0 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0.0f32; plane.len()];
    for _ in 0..3 {
        box_blur_horizontal(plane, &mut tmp, w, h, radius);
        box_blur_vertical(&tmp, plane, w, h, radius);
    }
}

/// Blur a packed `Xbgr2101010` (PQ) frame: unpack each 10-bit channel, PQ EOTF
/// to linear nits, blur, PQ OETF back, repack. Alpha is left at fully opaque
/// (`0b11`) to match `pack_rgb10`.
fn blur_pq_rgba(pixels: &[u8], w: usize, h: usize, radius: usize) -> Vec<u8> {
    let n = w * h;
    let mut r = vec![0.0f32; n];
    let mut g = vec![0.0f32; n];
    let mut b = vec![0.0f32; n];
    for (i, chunk) in pixels.chunks_exact(4).enumerate() {
        let packed = u32::from_le_bytes(chunk.try_into().expect("4-byte chunk"));
        r[i] = pq_eotf(((packed & 0x3ff) as f64) / 1023.0) as f32;
        g[i] = pq_eotf((((packed >> 10) & 0x3ff) as f64) / 1023.0) as f32;
        b[i] = pq_eotf((((packed >> 20) & 0x3ff) as f64) / 1023.0) as f32;
    }
    box_blur_plane(&mut r, w, h, radius);
    box_blur_plane(&mut g, w, h, radius);
    box_blur_plane(&mut b, w, h, radius);
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n {
        let code = |nits: f32| (pq_oetf(nits as f64) * 1023.0).round().clamp(0.0, 1023.0) as u32;
        let packed = code(r[i]) | (code(g[i]) << 10) | (code(b[i]) << 20) | (0b11 << 30);
        out.extend_from_slice(&packed.to_le_bytes());
    }
    out
}

/// Blur a packed `Xbgr8888` (sRGB) frame. Blurred in normalised linear light
/// rather than absolute nits -- an SDR wallpaper has no absolute scale to
/// preserve the way the PQ path does.
fn blur_srgb_rgba(pixels: &[u8], w: usize, h: usize, radius: usize) -> Vec<u8> {
    fn to_linear(v: f64) -> f64 {
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    }
    fn to_srgb(v: f64) -> f64 {
        if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
    }
    let n = w * h;
    let mut r = vec![0.0f32; n];
    let mut g = vec![0.0f32; n];
    let mut b = vec![0.0f32; n];
    for (i, chunk) in pixels.chunks_exact(4).enumerate() {
        r[i] = to_linear(chunk[0] as f64 / 255.0) as f32;
        g[i] = to_linear(chunk[1] as f64 / 255.0) as f32;
        b[i] = to_linear(chunk[2] as f64 / 255.0) as f32;
    }
    box_blur_plane(&mut r, w, h, radius);
    box_blur_plane(&mut g, w, h, radius);
    box_blur_plane(&mut b, w, h, radius);
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n {
        let code = |v: f32| (to_srgb(v.clamp(0.0, 1.0) as f64) * 255.0).round().clamp(0.0, 255.0) as u8;
        out.extend_from_slice(&[code(r[i]), code(g[i]), code(b[i]), 0xff]);
    }
    out
}

/// The blurred variant of one decoded frame, or `None` when frosting is off
/// (`radius == 0`) or the fourcc is not one blur knows how to unpack --
/// currently every fourcc this module produces is covered.
fn blur_frame(pixels: &[u8], size: Size<i32, BufferCoords>, fourcc: Fourcc, radius: u32) -> Option<Vec<u8>> {
    if radius == 0 {
        return None;
    }
    let (w, h) = (size.w.max(0) as usize, size.h.max(0) as usize);
    match fourcc {
        Fourcc::Xbgr2101010 => Some(blur_pq_rgba(pixels, w, h, radius as usize)),
        Fourcc::Xbgr8888 => Some(blur_srgb_rgba(pixels, w, h, radius as usize)),
        _ => None,
    }
}

/// Decode an image file into a [`Wallpaper`], dispatching on extension.
///
/// AVIF is the only format that carries a transfer function Rubix cares about,
/// so it is the only one that can yield a `DecodeKind` other than `Sdr`. PNG
/// and JPEG are always SDR by construction.
fn load(path: &Path, scale: f32, blur_radius: u32) -> Result<Wallpaper, String> {
    Ok(Wallpaper::new(decode_file(path, scale, blur_radius)?))
}

/// The pure half of [`load`]: file in, pixels and colour metadata out.
///
/// `blur_radius` is applied here, after the format-specific decode, rather
/// than threaded into `load_avif`/`load_sdr`: both already hand back the
/// fully decoded first frame, and the blur itself does not care which decoder
/// produced the pixels, only their fourcc.
fn decode_file(path: &Path, scale: f32, blur_radius: u32) -> Result<Decoded, String> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut decoded = match extension.as_str() {
        "avif" => load_avif(path, scale)?,
        "png" | "jpg" | "jpeg" => load_sdr(path, scale)?,
        "" => return Err("no file extension; cannot tell what format this is".into()),
        other => return Err(format!("unsupported format .{other} (want .avif, .png, .jpg)")),
    };
    decoded.blurred = decoded
        .frames
        .first()
        .and_then(|frame| blur_frame(&frame.pixels, decoded.size, decoded.fourcc, blur_radius));
    Ok(decoded)
}

/// PNG/JPEG via the `image` crate. Always 8-bit sRGB.
fn load_sdr(path: &Path, scale: f32) -> Result<Decoded, String> {
    let image = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let size = Size::from((image.width() as i32, image.height() as i32));
    let mut pixels = image.into_raw();
    // Forced opaque to match the opaque region declared in `Wallpaper::new`.
    // A wallpaper has nothing behind it, so a transparent one would composite
    // against uninitialised framebuffer rather than anything meaningful.
    let gain = (!is_unity(scale)).then(|| srgb_gain_table(scale));
    for chunk in pixels.chunks_exact_mut(4) {
        if let Some(table) = &gain {
            for channel in &mut chunk[..3] {
                *channel = table[*channel as usize];
            }
        }
        chunk[3] = 0xff;
    }
    Ok(Decoded {
        frames: vec![DecodedFrame { pixels, duration: None }],
        size,
        fourcc: Fourcc::Xbgr8888,
        decode: DecodeKind::Sdr,
        blurred: None,
    })
}

/// Owns an `avifDecoder` so every early return frees it.
struct AvifDecoder(*mut libavif_sys::avifDecoder);

impl Drop for AvifDecoder {
    fn drop(&mut self) {
        unsafe { libavif_sys::avifDecoderDestroy(self.0) };
    }
}

/// Owns the pixel buffer `avifRGBImageAllocatePixels` hands back.
struct AvifRgb(libavif_sys::avifRGBImage);

impl Drop for AvifRgb {
    fn drop(&mut self) {
        unsafe { libavif_sys::avifRGBImageFreePixels(&mut self.0) };
    }
}

/// AVIF via libavif (dav1d). Reads the CICP block to decide the transfer
/// function, which is the whole reason this format is worth special-casing.
///
/// Only the first frame is decoded. Animated AVIF exposes the rest through
/// repeated `avifDecoderNextImage` calls with per-frame `imageTiming`, which is
/// what [`DecodedFrame::duration`] exists to hold -- but decoding every frame of
/// a 4K sequence at ~33 MB each needs a memory budget that is not designed yet.
fn load_avif(path: &Path, scale: f32) -> Result<Decoded, String> {
    use libavif_sys::*;

    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "path contains a NUL byte".to_string())?;

    unsafe {
        let decoder = AvifDecoder(avifDecoderCreate());
        if decoder.0.is_null() {
            return Err("could not create an AVIF decoder".into());
        }
        if avifDecoderSetIOFile(decoder.0, c_path.as_ptr()) != AVIF_RESULT_OK {
            return Err("could not open file".into());
        }
        if avifDecoderParse(decoder.0) != AVIF_RESULT_OK {
            return Err("not a readable AVIF".into());
        }
        if avifDecoderNextImage(decoder.0) != AVIF_RESULT_OK {
            return Err("no decodable image (is an AV1 decoder available?)".into());
        }
        let image = (*decoder.0).image;
        let size = Size::from(((*image).width as i32, (*image).height as i32));
        let hdr = (*image).transferCharacteristics as u32 == AVIF_TRANSFER_CHARACTERISTICS_SMPTE2084;

        if hdr && (*image).colorPrimaries as u32 != AVIF_COLOR_PRIMARIES_BT2020 {
            // DECODE_HDR_PQ treats its input as BT.2020 and passes the
            // primaries straight through, so anything else lands in the wrong
            // gamut. Rare enough (PQ outside BT.2020 is nearly unheard of) that
            // converting is not worth it, but silently mis-rendering is worse
            // than saying so.
            tracing::warn!(
                "wallpaper {}: PQ transfer with non-BT.2020 primaries ({}); colours will be off",
                path.display(),
                (*image).colorPrimaries as u32,
            );
        }

        let mut rgb = AvifRgb(std::mem::zeroed());
        avifRGBImageSetDefaults(&mut rgb.0, image);
        rgb.0.format = AVIF_RGB_FORMAT_RGBA;
        rgb.0.depth = if hdr { 10 } else { 8 };
        // The image is composited as opaque (see `Wallpaper::new`), so alpha is
        // dropped at the source rather than premultiplied and then ignored.
        rgb.0.ignoreAlpha = 1;
        if avifRGBImageAllocatePixels(&mut rgb.0) != AVIF_RESULT_OK {
            return Err("could not allocate pixels for the decoded image".into());
        }
        if avifImageYUVToRGB(image, &mut rgb.0) != AVIF_RESULT_OK {
            return Err("YUV to RGB conversion failed".into());
        }

        let (pixels, fourcc, decode) = if hdr {
            let gain = (!is_unity(scale)).then(|| pq_gain_table(scale));
            (
                pack_rgb10(&rgb.0, gain.as_ref()),
                Fourcc::Xbgr2101010,
                DecodeKind::HdrPq,
            )
        } else {
            let gain = (!is_unity(scale)).then(|| srgb_gain_table(scale));
            (pack_rgb8(&rgb.0, gain.as_ref()), Fourcc::Xbgr8888, DecodeKind::Sdr)
        };
        Ok(Decoded {
            frames: vec![DecodedFrame { pixels, duration: None }],
            size,
            fourcc,
            decode,
            blurred: None,
        })
    }
}

/// Pack libavif's 10-bit RGBA (one `u16` per channel) into `Xbgr2101010`.
///
/// The DRM format is a little-endian `u32` with red in the low ten bits and the
/// two ignored alpha bits at the top, which is exactly what GL's
/// `UNSIGNED_INT_2_10_10_10_REV` reads (smithay maps the fourcc to
/// `RGB10_A2`/`RGBA`/`UNSIGNED_INT_2_10_10_10_REV`). Alpha is written as 3
/// (fully opaque) rather than left at whatever `ignoreAlpha` produced.
///
/// # Safety
/// `rgb.pixels` must be a valid allocation of `rowBytes * height` bytes holding
/// 10-bit samples, as produced by `avifRGBImageAllocatePixels` at `depth = 10`.
unsafe fn pack_rgb10(rgb: &libavif_sys::avifRGBImage, gain: Option<&[u16; 1024]>) -> Vec<u8> {
    let (width, height) = (rgb.width as usize, rgb.height as usize);
    let mut out = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let row = unsafe { rgb.pixels.add(y * rgb.rowBytes as usize) } as *const u16;
        for x in 0..width {
            let sample = |channel: usize| {
                let raw = unsafe { *row.add(x * 4 + channel) } as usize & 0x3ff;
                match gain {
                    Some(table) => table[raw] as u32,
                    None => raw as u32,
                }
            };
            let packed = sample(0) | (sample(1) << 10) | (sample(2) << 20) | (0b11 << 30);
            out.extend_from_slice(&packed.to_le_bytes());
        }
    }
    out
}

/// Copy libavif's 8-bit RGBA out row by row, forcing alpha opaque.
/// `rowBytes` is not necessarily `width * 4`, so this cannot be one memcpy.
///
/// # Safety
/// Same contract as [`pack_rgb10`], at `depth = 8`.
unsafe fn pack_rgb8(rgb: &libavif_sys::avifRGBImage, gain: Option<&[u8; 256]>) -> Vec<u8> {
    let (width, height) = (rgb.width as usize, rgb.height as usize);
    let mut out = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let row = unsafe { rgb.pixels.add(y * rgb.rowBytes as usize) };
        let slice = unsafe { std::slice::from_raw_parts(row, width * 4) };
        for pixel in slice.chunks_exact(4) {
            let map = |v: u8| match gain {
                Some(table) => table[v as usize],
                None => v,
            };
            out.extend_from_slice(&[map(pixel[0]), map(pixel[1]), map(pixel[2]), 0xff]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures")).join(name)
    }

    fn buffer(w: i32, h: i32) -> Size<i32, BufferCoords> {
        Size::from((w, h))
    }

    fn output(w: i32, h: i32) -> Size<i32, Logical> {
        Size::from((w, h))
    }

    // --- placement -------------------------------------------------------

    #[test]
    fn stretch_uses_the_whole_image_and_the_whole_output() {
        let p = place(WallpaperMode::Stretch, buffer(100, 100), output(200, 50));
        assert_eq!(p.src, Rectangle::from_size(Size::from((100.0, 100.0))));
        assert_eq!(p.size, output(200, 50));
        assert_eq!(p.loc, Point::default());
    }

    #[test]
    fn fill_crops_the_overflowing_axis_and_leaves_no_gap() {
        // 100x100 onto 200x100: scale 2.0 makes it 200x200, so half the height
        // is cropped -- 50 buffer pixels, centred.
        let p = place(WallpaperMode::Fill, buffer(100, 100), output(200, 100));
        assert_eq!(p.src.size, Size::from((100.0, 50.0)));
        assert_eq!(p.src.loc, Point::from((0.0, 25.0)));
        // The whole output is covered, which is the point of Fill.
        assert_eq!(p.size, output(200, 100));
        assert_eq!(p.loc, Point::default());
    }

    #[test]
    fn fill_crops_the_other_axis_when_the_image_is_the_wide_one() {
        // Same test mirrored: the failure mode of getting the max/min backwards
        // is a wallpaper that fits instead of filling, which is easy to miss.
        let p = place(WallpaperMode::Fill, buffer(200, 100), output(100, 100));
        assert_eq!(p.src.size, Size::from((100.0, 100.0)));
        assert_eq!(p.src.loc, Point::from((50.0, 0.0)));
        assert_eq!(p.size, output(100, 100));
    }

    #[test]
    fn fit_letterboxes_and_centres() {
        // 100x100 onto 200x100: scale 1.0 (the smaller ratio), so 100x100
        // centred horizontally with 50px bars either side.
        let p = place(WallpaperMode::Fit, buffer(100, 100), output(200, 100));
        assert_eq!(p.src, Rectangle::from_size(Size::from((100.0, 100.0))));
        assert_eq!(p.size, output(100, 100));
        assert_eq!(p.loc, Point::from((50, 0)));
    }

    #[test]
    fn center_crops_a_large_image_without_scaling_it() {
        let p = place(WallpaperMode::Center, buffer(200, 200), output(100, 100));
        assert_eq!(p.src.size, Size::from((100.0, 100.0)));
        assert_eq!(p.src.loc, Point::from((50.0, 50.0)));
        assert_eq!(p.size, output(100, 100));
        assert_eq!(p.loc, Point::default());
    }

    #[test]
    fn center_pads_a_small_image_without_scaling_it() {
        let p = place(WallpaperMode::Center, buffer(50, 50), output(100, 100));
        assert_eq!(p.src, Rectangle::from_size(Size::from((50.0, 50.0))));
        assert_eq!(p.size, output(50, 50));
        assert_eq!(p.loc, Point::from((25, 25)));
    }

    #[test]
    fn center_handles_an_image_larger_on_one_axis_and_smaller_on_the_other() {
        // The case that catches an implementation which picks one behaviour for
        // the whole image instead of deciding per axis.
        let p = place(WallpaperMode::Center, buffer(200, 50), output(100, 100));
        assert_eq!(p.src.size, Size::from((100.0, 50.0)));
        assert_eq!(p.src.loc, Point::from((50.0, 0.0)));
        assert_eq!(p.size, output(100, 50));
        assert_eq!(p.loc, Point::from((0, 25)));
    }

    #[test]
    fn a_zero_sized_output_does_not_divide_by_zero() {
        // An output can report 0x0 briefly during a mode change; every mode
        // divides by one of these, so all four are checked.
        for mode in [
            WallpaperMode::Fill,
            WallpaperMode::Fit,
            WallpaperMode::Stretch,
            WallpaperMode::Center,
        ] {
            let p = place(mode, buffer(0, 0), output(0, 0));
            assert!(p.size.w >= 0 && p.size.h >= 0, "{mode:?} produced {:?}", p.size);
        }
    }

    // --- packing ---------------------------------------------------------

    #[test]
    fn ten_bit_packing_puts_red_low_and_opaque_alpha_high() {
        // Xbgr2101010 is a little-endian u32: red in bits 0..10, green 10..20,
        // blue 20..30, alpha 30..32. Getting this reversed swaps red and blue
        // in every HDR wallpaper -- visible, but easy to blame on the shader.
        let mut samples: Vec<u16> = vec![1023, 512, 0, 0];
        let mut rgb: libavif_sys::avifRGBImage = unsafe { std::mem::zeroed() };
        rgb.width = 1;
        rgb.height = 1;
        rgb.depth = 10;
        rgb.rowBytes = 8;
        rgb.pixels = samples.as_mut_ptr() as *mut u8;

        let packed = unsafe { pack_rgb10(&rgb, None) };
        let word = u32::from_le_bytes(packed.try_into().unwrap());
        assert_eq!(word & 0x3ff, 1023, "red");
        assert_eq!((word >> 10) & 0x3ff, 512, "green");
        assert_eq!((word >> 20) & 0x3ff, 0, "blue");
        assert_eq!(word >> 30, 0b11, "alpha must be forced opaque");
    }

    #[test]
    fn packing_respects_row_padding() {
        // rowBytes is not always width * bytes-per-pixel. Reading rows as one
        // contiguous run would shear the image progressively down the frame.
        let mut samples: Vec<u16> = vec![
            1, 0, 0, 0, /* pad: */ 0, 0, 0, 0, // row 0, one pixel + slack
            2, 0, 0, 0, /* pad: */ 0, 0, 0, 0, // row 1
        ];
        let mut rgb: libavif_sys::avifRGBImage = unsafe { std::mem::zeroed() };
        rgb.width = 1;
        rgb.height = 2;
        rgb.depth = 10;
        rgb.rowBytes = 16;
        rgb.pixels = samples.as_mut_ptr() as *mut u8;

        let packed = unsafe { pack_rgb10(&rgb, None) };
        assert_eq!(packed.len(), 8);
        assert_eq!(u32::from_le_bytes(packed[0..4].try_into().unwrap()) & 0x3ff, 1);
        assert_eq!(u32::from_le_bytes(packed[4..8].try_into().unwrap()) & 0x3ff, 2);
    }




    #[test]
    fn the_sdr_reference_is_live_and_independent_of_the_decode_gain() {
        // It only feeds a shader uniform, so unlike `luminance_scale` it must
        // take effect without re-decoding anything -- which is also what makes
        // it safe to tune while a slideshow is running.
        let mut manager = WallpaperManager::default();
        let mut config = slideshow_config(&fixture("pq16.avif"), 300);
        assert!(manager.resolve(&config, &[], &crate::config::DecorationConfig::default()).is_empty());
        let decoded = manager.for_output("DP-3").unwrap().frames[0].pixels.clone();

        config.sdr_reference_nits = 2000.0;
        assert!(manager.resolve(&config, &[], &crate::config::DecorationConfig::default()).is_empty());
        assert_eq!(manager.sdr_reference_nits, 2000.0);
        assert_eq!(
            manager.for_output("DP-3").unwrap().frames[0].pixels,
            decoded,
            "changing the SDR reference must not re-decode",
        );
    }

    #[test]
    fn a_backdrop_on_the_hdr_pass_always_carries_a_program() {
        // The HDR composite pass installs no pass-wide default, so every arm
        // with `hdr_pass = true` must be `Some`. A `None` here draws the quad
        // with the previous element's program.
        for kind in [DecodeKind::Sdr, DecodeKind::HdrPq, DecodeKind::WindowsScrgb] {
            for tonemap in [false, true] {
                assert!(
                    backdrop_program(tonemap, kind, true, false).is_some(),
                    "{kind:?} tonemap={tonemap} on the HDR pass has no program"
                );
            }
        }
    }

    #[test]
    fn an_untonemapped_backdrop_decodes_rather_than_tonemapping() {
        // The two configs that reach this: blur on with tonemap off, and any
        // SDR wallpaper on an HDR output.
        assert_eq!(
            backdrop_program(false, DecodeKind::HdrPq, true, false),
            Some(RoundMode::Decode(DecodeKind::HdrPq))
        );
        assert_eq!(
            backdrop_program(true, DecodeKind::Sdr, true, false),
            Some(RoundMode::Decode(DecodeKind::Sdr)),
            "an SDR wallpaper has nothing to tone-map, so it takes a plain decode"
        );
    }

    #[test]
    fn the_tonemap_destination_decides_sdr_versus_abs10k() {
        // Backwards here reads on screen as the backdrop crushing to white --
        // shipped once already.
        assert_eq!(
            backdrop_program(true, DecodeKind::HdrPq, true, false),
            Some(RoundMode::TonemapAbs10k(DecodeKind::HdrPq)),
            "the HDR pass must stay in the abs10k working space"
        );
        assert_eq!(
            backdrop_program(true, DecodeKind::HdrPq, false, false),
            Some(RoundMode::Tonemap(DecodeKind::HdrPq)),
            "an sRGB destination must collapse to it"
        );
    }

    #[test]
    fn an_sdr_destination_leaves_an_untonemapped_backdrop_alone() {
        assert_eq!(backdrop_program(false, DecodeKind::HdrPq, false, false), None);
        assert_eq!(backdrop_program(false, DecodeKind::Sdr, false, false), None);
    }

    #[test]
    fn a_refracting_backdrop_always_carries_a_program() {
        // A refracting quad needs the sample rewrite even when the colour path
        // wants nothing: the (false, false) arm returns `None` without
        // refraction and `Some(RoundMode::Plain)` with it.
        assert_eq!(
            backdrop_program(false, DecodeKind::Sdr, false, false),
            None
        );
        assert_eq!(
            backdrop_program(false, DecodeKind::Sdr, false, true),
            Some(RoundMode::Plain)
        );
    }

    #[test]
    fn backdrop_reference_comes_from_backdrop_luminance_nits_not_sdr_reference_nits() {
        // The bug this guards: the backdrop borrowed `sdr_reference_nits`,
        // which is deliberately large (it normalises the whole wallpaper for
        // an SDR output). The tone curve is identity below 0.8x its
        // reference, so borrowing it put the knee above the wallpaper's peak
        // and the cap became an exact no-op. The two must stay independent.
        let mut manager = WallpaperManager::default();
        let mut config = slideshow_config(&fixture("pq16.avif"), 300);
        config.sdr_reference_nits = 2000.0;
        let decoration = crate::config::DecorationConfig {
            backdrop_luminance_nits: 40.0,
            ..crate::config::DecorationConfig::default()
        };
        assert!(manager.resolve(&config, &[], &decoration).is_empty());
        assert_eq!(manager.backdrop_luminance_nits, 40.0);
        assert_eq!(manager.sdr_reference_nits, 2000.0);
    }

    #[test]
    fn backdrop_ceiling_is_display_referred_and_ignores_the_decode_gain() {
        // `sdr_reference_nits` is measured pre-gain and must be scaled by it;
        // `backdrop_luminance_nits` is an absolute on-screen ceiling and must
        // NOT be. Scaling it twice put a 40-nit ceiling at 8 nits, dropping
        // the knee to the wallpaper's median and flattening half the image to
        // a narrow band -- which read on screen as the colour being crushed.
        let mut manager = WallpaperManager::default();
        let mut config = slideshow_config(&fixture("pq16.avif"), 300);
        config.luminance_scale = 0.2;
        let decoration = crate::config::DecorationConfig {
            backdrop_luminance_nits: 40.0,
            ..crate::config::DecorationConfig::default()
        };
        assert!(manager.resolve(&config, &[], &decoration).is_empty());
        assert_eq!(manager.scale, 0.2, "gain is applied to the texels");
        assert_eq!(
            manager.backdrop_luminance_nits, 40.0,
            "the ceiling stays in display nits regardless of the decode gain",
        );
    }

    // --- slideshow --------------------------------------------------------

    /// A throwaway directory holding copies of the fixtures, removed on drop.
    /// Never a real wallpaper folder -- these tests delete what they create.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rubix-wallpaper-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id(),
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        fn put(&self, name: &str, fixture_name: &str) -> PathBuf {
            let dst = self.0.join(name);
            std::fs::copy(fixture(fixture_name), &dst).expect("copy fixture");
            dst
        }

        fn touch(&self, name: &str) -> PathBuf {
            let dst = self.0.join(name);
            std::fs::write(&dst, b"not an image").expect("write");
            dst
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn slideshow_config(path: &Path, interval: u64) -> crate::config::WallpaperConfig {
        crate::config::WallpaperConfig {
            path: Some(path.to_path_buf()),
            mode: WallpaperMode::Fill,
            luminance_scale: 1.0,
            interval_seconds: interval,
            sdr_reference_nits: 203.0,
        }
    }

    #[test]
    fn scanning_a_directory_sorts_and_filters() {
        let dir = TempDir::new("scan");
        dir.put("b.avif", "pq16.avif");
        dir.put("a.avif", "sdr16.avif");
        dir.touch("notes.txt");
        dir.touch("thumb.gif");

        let entries = scan_directory(&dir.0).expect("directory scans");
        let names: Vec<String> = entries
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Sorted, so the order is the same on every start -- a directory
        // listing has no inherent order.
        assert_eq!(names, vec!["a.avif", "b.avif"]);
    }

    #[test]
    fn a_directory_with_no_images_is_an_error_not_an_empty_slideshow() {
        // An empty entry list would panic on `entries[0]`; better to say so.
        let dir = TempDir::new("empty");
        dir.touch("readme.md");
        assert!(scan_directory(&dir.0).is_err());
    }

    #[test]
    fn a_directory_becomes_a_slideshow_and_a_file_does_not() {
        let dir = TempDir::new("mode");
        dir.put("a.avif", "pq16.avif");
        dir.put("b.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        assert!(manager.resolve(&slideshow_config(&dir.0, 5), &[], &crate::config::DecorationConfig::default()).is_empty());
        assert!(manager.slideshow.is_some());
        assert!(manager.next_slideshow_at().is_some());

        let mut single = WallpaperManager::default();
        let file = fixture("pq16.avif");
        assert!(single.resolve(&slideshow_config(&file, 5), &[], &crate::config::DecorationConfig::default()).is_empty());
        assert!(single.slideshow.is_none());
        // A single wallpaper must arm no timer at all -- a static desktop
        // should cost nothing.
        assert!(single.next_slideshow_at().is_none());
    }

    #[test]
    fn a_slideshow_starts_on_the_first_image_in_order() {
        let dir = TempDir::new("first");
        dir.put("b.avif", "sdr16.avif");
        dir.put("a.avif", "pq16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 5), &[], &crate::config::DecorationConfig::default());
        // "a.avif" is the PQ one, so this also confirms the right file loaded.
        assert!(manager.output_has_hdr("DP-3"));
    }

    #[test]
    fn a_swap_before_the_interval_elapses_does_nothing() {
        let dir = TempDir::new("early");
        dir.put("a.avif", "pq16.avif");
        dir.put("b.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 300), &[], &crate::config::DecorationConfig::default());
        assert!(!manager.advance_slideshow(Instant::now()));
    }

    #[test]
    fn a_swap_with_no_prefetch_ready_keeps_the_current_image() {
        // The compositor must never block on a decode. Without a decode
        // channel nothing is ever prefetched, which is exactly the "not ready"
        // case -- the swap is skipped and retried, not waited on.
        let dir = TempDir::new("notready");
        dir.put("a.avif", "pq16.avif");
        dir.put("b.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        let due = Instant::now() + Duration::from_secs(2);
        assert!(!manager.advance_slideshow(due));
        // Still on the PQ first image, and rescheduled for a short retry
        // rather than pushed out by a whole interval.
        assert!(manager.output_has_hdr("DP-3"));
        let next = manager.next_slideshow_at().unwrap();
        assert!(next <= due + Duration::from_millis(500), "retry must be soon");
    }

    #[test]
    fn a_delivered_prefetch_is_promoted_on_the_next_swap() {
        let dir = TempDir::new("promote");
        dir.put("a.avif", "pq16.avif");
        let b = dir.put("b.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        assert!(manager.output_has_hdr("DP-3"), "starts on the PQ image");

        manager.receive_prefetch(PrefetchedWallpaper {
            path: b,
            scale: 1.0,
            blur_radius: 0,
            result: decode_file(&fixture("sdr16.avif"), 1.0, 0),
        });
        assert!(manager.advance_slideshow(Instant::now() + Duration::from_secs(2)));
        assert!(!manager.output_has_hdr("DP-3"), "now on the SDR image");
    }

    #[test]
    fn a_prefetch_decoded_at_a_stale_gain_is_discarded() {
        // A config save between request and delivery would otherwise show one
        // image at the old brightness until the next swap.
        let dir = TempDir::new("stalegain");
        dir.put("a.avif", "pq16.avif");
        let b = dir.put("b.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        manager.receive_prefetch(PrefetchedWallpaper {
            path: b,
            scale: 0.5, // manager is at 1.0
            blur_radius: 0,
            result: decode_file(&fixture("sdr16.avif"), 0.5, 0),
        });
        assert!(manager.prefetch.is_none(), "stale gain must not be queued");
    }

    #[test]
    fn an_undecodable_image_is_dropped_from_the_rotation() {
        // One bad file in a folder of a hundred must not stall the slideshow on
        // it forever.
        let dir = TempDir::new("bad");
        dir.put("a.avif", "pq16.avif");
        let bad = dir.touch("b.avif");
        dir.put("c.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        assert_eq!(manager.slideshow.as_ref().unwrap().entries.len(), 3);

        manager.receive_prefetch(PrefetchedWallpaper {
            path: bad,
            scale: 1.0,
            blur_radius: 0,
            result: Err("not an image".into()),
        });
        let slideshow = manager.slideshow.as_ref().unwrap();
        assert_eq!(slideshow.entries.len(), 2);
        assert!(slideshow.index < slideshow.entries.len(), "index left in range");
    }

    #[test]
    fn a_slideshow_holds_at_most_the_current_image_and_one_prefetch() {
        // 115 wallpapers at ~33 MB each is not a cache that can be allowed to
        // grow.
        let dir = TempDir::new("memory");
        dir.put("a.avif", "pq16.avif");
        let b = dir.put("b.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        manager.receive_prefetch(PrefetchedWallpaper {
            path: b,
            scale: 1.0,
            blur_radius: 0,
            result: decode_file(&fixture("sdr16.avif"), 1.0, 0),
        });
        manager.advance_slideshow(Instant::now() + Duration::from_secs(2));
        assert_eq!(manager.loaded.len(), 1, "the previous image must be evicted");
    }

    #[test]
    fn a_single_image_folder_never_tries_to_prefetch_itself() {
        let dir = TempDir::new("lonely");
        dir.put("only.avif", "pq16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        assert!(manager.inflight.is_none());
        assert!(!manager.advance_slideshow(Instant::now() + Duration::from_secs(2)));
        assert!(manager.output_has_hdr("DP-3"));
    }


    #[test]
    fn a_reload_keeps_showing_the_current_image() {
        // Tuning `luminance_scale` means saving repeatedly, and every save
        // re-resolves. Restarting the rotation each time would make the setting
        // impossible to judge -- you would be comparing different pictures.
        let dir = TempDir::new("keepplace");
        dir.put("a.avif", "pq16.avif");
        let b = dir.put("b.avif", "sdr16.avif");

        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        manager.receive_prefetch(PrefetchedWallpaper {
            path: b.clone(),
            scale: 1.0,
            blur_radius: 0,
            result: decode_file(&fixture("sdr16.avif"), 1.0, 0),
        });
        assert!(manager.advance_slideshow(Instant::now() + Duration::from_secs(2)));
        assert_eq!(manager.slideshow.as_ref().unwrap().index, 1);

        // A save that changes only the gain.
        let mut retuned = slideshow_config(&dir.0, 1);
        retuned.luminance_scale = 0.5;
        manager.resolve(&retuned, &[], &crate::config::DecorationConfig::default());
        assert_eq!(manager.slideshow.as_ref().unwrap().index, 1, "rotation restarted");
        assert_eq!(manager.default_path.as_ref(), Some(&b));
    }

    #[test]
    fn a_reload_picks_up_images_added_to_the_folder() {
        // The rescan is what makes the index lookup by path rather than by
        // number: inserting a file earlier in sort order shifts every index.
        let dir = TempDir::new("added");
        dir.put("b.avif", "pq16.avif");
        let mut manager = WallpaperManager::default();
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        assert_eq!(manager.slideshow.as_ref().unwrap().entries.len(), 1);

        dir.put("a.avif", "sdr16.avif");
        manager.resolve(&slideshow_config(&dir.0, 1), &[], &crate::config::DecorationConfig::default());
        let slideshow = manager.slideshow.as_ref().unwrap();
        assert_eq!(slideshow.entries.len(), 2, "new file not picked up");
        // Still showing b.avif, now at index 1 rather than 0.
        assert_eq!(slideshow.index, 1);
        assert!(manager.output_has_hdr("DP-3"), "still the PQ image");
    }

    // --- luminance gain ---------------------------------------------------

    #[test]
    fn pq_transfer_round_trips() {
        // The gain table is built from these two, so an error in either shifts
        // every wallpaper's brightness without ever failing loudly.
        for nits in [0.0, 0.1, 1.0, 100.0, 203.0, 604.0, 1000.0, 4000.0, 10000.0] {
            let round_tripped = pq_eotf(pq_oetf(nits));
            assert!(
                (round_tripped - nits).abs() < nits.max(1.0) * 1e-3,
                "{nits} nits round-tripped to {round_tripped}",
            );
        }
    }

    #[test]
    fn pq_reference_points_match_the_standard() {
        // ST 2084 anchors: signal 1.0 is 10000 nits, and 100 nits sits at the
        // well-known 0.5081 signal level.
        assert!((pq_eotf(1.0) - 10000.0).abs() < 1.0);
        assert!(pq_eotf(0.0) < 1e-6);
        assert!((pq_oetf(100.0) - 0.5081).abs() < 1e-3, "{}", pq_oetf(100.0));
    }

    #[test]
    fn a_unity_gain_table_is_the_identity() {
        // Unity is skipped at runtime via `is_unity`, but if it were applied it
        // must not shift anything -- that property is what makes the table
        // trustworthy at every other scale.
        let table = pq_gain_table(1.0);
        for code in [0usize, 1, 512, 940, 1023] {
            assert!(
                (table[code] as i32 - code as i32).abs() <= 1,
                "code {code} mapped to {}",
                table[code],
            );
        }
        let srgb = srgb_gain_table(1.0);
        for code in [0usize, 1, 128, 255] {
            assert!((srgb[code] as i32 - code as i32).abs() <= 1);
        }
    }

    #[test]
    fn halving_the_gain_halves_the_luminance() {
        // The gain is defined in linear light, not in signal -- halving a PQ
        // code would be a completely different (and wrong) curve.
        let table = pq_gain_table(0.5);
        for code in [200usize, 512, 800, 1000] {
            let before = pq_eotf(code as f64 / 1023.0);
            let after = pq_eotf(table[code] as f64 / 1023.0);
            assert!(
                (after - before * 0.5).abs() < before * 0.01 + 0.01,
                "code {code}: {before} nits -> {after}, wanted {}",
                before * 0.5,
            );
        }
    }

    #[test]
    fn a_gain_never_pushes_a_code_out_of_range() {
        // Both directions: a gain above 1.0 must clamp at the top rather than
        // wrapping, and black must stay black at every scale.
        for scale in [0.05f32, 0.5, 1.0, 2.0, 4.0] {
            let table = pq_gain_table(scale);
            assert!(table.iter().all(|&c| c <= 1023), "scale {scale} overflowed");
            assert_eq!(table[0], 0, "scale {scale} lifted black");
            let srgb = srgb_gain_table(scale);
            assert_eq!(srgb[0], 0, "scale {scale} lifted sRGB black");
        }
    }

    #[test]
    fn decoding_at_a_lower_gain_darkens_the_image() {
        // End to end through the real decoder, on the real fixture.
        let bright = decode_file(&fixture("pq16.avif"), 1.0, 0).unwrap();
        let dim = decode_file(&fixture("pq16.avif"), 0.25, 0).unwrap();
        let red = |d: &Decoded| {
            u32::from_le_bytes(d.frames[0].pixels[0..4].try_into().unwrap()) & 0x3ff
        };
        assert!(red(&dim) < red(&bright), "{} !< {}", red(&dim), red(&bright));
    }

    #[test]
    fn changing_the_gain_re_decodes_cached_images() {
        // The gain is baked into the pixels, so a cache that survived a scale
        // change would silently keep showing the old brightness.
        let mut manager = WallpaperManager::default();
        let config = crate::config::WallpaperConfig {
            path: Some(fixture("pq16.avif")),
            mode: WallpaperMode::Fill,
            luminance_scale: 1.0,
            interval_seconds: 300,
            sdr_reference_nits: 203.0,
        };
        assert!(manager.resolve(&config, &[], &crate::config::DecorationConfig::default()).is_empty());
        let before = manager.for_output("DP-3").unwrap().frames[0].pixels.clone();

        let dimmed = crate::config::WallpaperConfig { luminance_scale: 0.25, ..config };
        assert!(manager.resolve(&dimmed, &[], &crate::config::DecorationConfig::default()).is_empty());
        let after = manager.for_output("DP-3").unwrap().frames[0].pixels.clone();
        assert_ne!(before, after, "a gain change must re-decode");
    }

    #[test]
    fn a_default_manager_starts_at_unity_not_zero() {
        // `derive(Default)` would give 0.0 here and decode every wallpaper to
        // black -- a failure that looks exactly like "the wallpaper is broken".
        assert_eq!(WallpaperManager::default().scale, 1.0);
    }

    // --- backdrop blur -----------------------------------------------------

    #[test]
    fn box_blur_line_radius_zero_is_identity() {
        let src = [1.0f32, 5.0, -2.0, 9.0, 0.0];
        let mut dst = [0.0f32; 5];
        box_blur_line(&src, &mut dst, 0);
        assert_eq!(dst, src);
    }

    #[test]
    fn box_blur_line_matches_hand_computed_output() {
        // [0,0,0,10,0,0,0] at radius 1: every window is 3 wide (clamped at the
        // edges), so only the samples straddling the spike see anything.
        let src = [0.0f32, 0.0, 0.0, 10.0, 0.0, 0.0, 0.0];
        let mut dst = [0.0f32; 7];
        box_blur_line(&src, &mut dst, 1);
        let expected = [0.0, 0.0, 10.0 / 3.0, 10.0 / 3.0, 10.0 / 3.0, 0.0, 0.0];
        for (got, want) in dst.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-6, "{dst:?} != {expected:?}");
        }
    }

    #[test]
    fn box_blur_plane_radius_zero_is_identity() {
        let mut plane = vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let before = plane.clone();
        box_blur_plane(&mut plane, 3, 2, 0);
        assert_eq!(plane, before);
    }

    #[test]
    fn box_blur_plane_is_separable_and_symmetric() {
        // A symmetric input (a single spike dead centre of an odd x odd plane)
        // must stay symmetric after blurring -- a horizontal/vertical pass
        // order bug tends to show up as an asymmetric smear instead.
        let (w, h) = (5usize, 5usize);
        let mut plane = vec![0.0f32; w * h];
        plane[(h / 2) * w + (w / 2)] = 100.0;
        box_blur_plane(&mut plane, w, h, 1);
        for y in 0..h {
            for x in 0..w {
                let mirrored = plane[(h - 1 - y) * w + (w - 1 - x)];
                let value = plane[y * w + x];
                assert!(
                    (mirrored - value).abs() < 1e-4,
                    "({x},{y})={value} != mirror {mirrored}"
                );
            }
        }
        // And a real blur, not a no-op: the centre must have spread outward.
        assert!(plane[(h / 2) * w + (w / 2)] < 100.0);
        assert!(plane[(h / 2) * w + (w / 2) - 1] > 0.0);
    }

    #[test]
    fn blur_pq_rgba_with_zero_radius_round_trips_within_tolerance() {
        // No blur happening (radius 0), so this isolates the PQ EOTF/OETF
        // round trip plus the 10-bit repack -- a drift here would mean every
        // frosted wallpaper is subtly discoloured even with radius 0 filtered
        // out at the call site.
        let w = 5usize;
        let h = 1usize;
        let mut pixels = Vec::new();
        for code in [0u32, 200, 512, 800, 1023] {
            let packed = code | (code << 10) | (code << 20) | (0b11 << 30);
            pixels.extend_from_slice(&packed.to_le_bytes());
        }
        let out = blur_pq_rgba(&pixels, w, h, 0);
        for i in 0..w {
            let original = u32::from_le_bytes(pixels[i * 4..i * 4 + 4].try_into().unwrap());
            let round_tripped = u32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
            let orig_r = original & 0x3ff;
            let rt_r = round_tripped & 0x3ff;
            assert!(
                orig_r.abs_diff(rt_r) <= 1,
                "code {orig_r} round-tripped to {rt_r}"
            );
        }
    }

    #[test]
    fn blur_frame_is_none_at_radius_zero() {
        let pixels = vec![0u8; 4];
        assert!(blur_frame(&pixels, buffer(1, 1), Fourcc::Xbgr2101010, 0).is_none());
    }

    #[test]
    fn blur_frame_blurs_pq_and_srgb_but_not_other_fourccs() {
        let pixels_pq = vec![0u8; 4 * 3 * 3];
        assert!(blur_frame(&pixels_pq, buffer(3, 3), Fourcc::Xbgr2101010, 1).is_some());
        let pixels_srgb = vec![0u8; 4 * 3 * 3];
        assert!(blur_frame(&pixels_srgb, buffer(3, 3), Fourcc::Xbgr8888, 1).is_some());
        assert!(blur_frame(&pixels_pq, buffer(3, 3), Fourcc::Argb8888, 1).is_none());
    }

    // --- backdrop crop -----------------------------------------------------

    #[test]
    fn the_refraction_mapping_undoes_the_crop_to_window_local_pixels() {
        // A window sitting well away from the wallpaper's origin, which is the
        // case that was broken: at the origin the bug is invisible.
        let crop = Rectangle::<f64, Logical>::new(
            Point::from((1200.0, 700.0)),
            Size::from((640.0, 360.0)),
        );
        let buffer = (3840.0, 2160.0);
        let dst = (1280.0, 720.0);
        let (origin, per_px) = refraction_mapping(crop, buffer, dst);

        // Top-left of the crop must land on window pixel (0, 0).
        let u0 = (crop.loc.x / buffer.0) as f32;
        let v0 = (crop.loc.y / buffer.1) as f32;
        assert!(((u0 - origin.0) / per_px.0).abs() < 1e-3);
        assert!(((v0 - origin.1) / per_px.1).abs() < 1e-3);

        // Bottom-right must land on the quad's size in physical pixels.
        let u1 = ((crop.loc.x + crop.size.w) / buffer.0) as f32;
        let v1 = ((crop.loc.y + crop.size.h) / buffer.1) as f32;
        assert!((((u1 - origin.0) / per_px.0) - dst.0 as f32).abs() < 1e-1);
        assert!((((v1 - origin.1) / per_px.1) - dst.1 as f32).abs() < 1e-1);
    }

    #[test]
    fn a_window_that_moves_keeps_its_own_facet_coordinates() {
        // The same window at two positions over the wallpaper. Window-local
        // coordinates must come out identical, or the facets slide.
        let buffer = (3840.0, 2160.0);
        let dst = (900.0, 560.0);
        let size = Size::from((450.0, 280.0));
        let a = Rectangle::<f64, Logical>::new(Point::from((300.0, 400.0)), size);
        let b = Rectangle::<f64, Logical>::new(Point::from((2100.0, 900.0)), size);
        let (oa, pa) = refraction_mapping(a, buffer, dst);
        let (ob, pb) = refraction_mapping(b, buffer, dst);

        for frac in [0.0f32, 0.25, 0.5, 1.0] {
            let ua = (a.loc.x / buffer.0) as f32 + frac * pa.0 * dst.0 as f32;
            let ub = (b.loc.x / buffer.0) as f32 + frac * pb.0 * dst.0 as f32;
            let xa = (ua - oa.0) / pa.0;
            let xb = (ub - ob.0) / pb.0;
            assert!((xa - xb).abs() < 1e-1, "frac {frac}: {xa} vs {xb}");
        }
    }

    #[test]
    fn crop_for_window_covering_the_whole_output_recovers_placements_src() {
        for mode in [
            WallpaperMode::Fill,
            WallpaperMode::Fit,
            WallpaperMode::Stretch,
            WallpaperMode::Center,
        ] {
            let placement = place(mode, buffer(200, 100), output(200, 100));
            let whole = Rectangle::<i32, Logical>::new(placement.loc, placement.size);
            let crop = crop_for_window(&placement, whole);
            assert!(
                (crop.loc.x - placement.src.loc.x).abs() < 1e-6
                    && (crop.loc.y - placement.src.loc.y).abs() < 1e-6,
                "{mode:?}: {crop:?} != {:?}",
                placement.src
            );
            assert!(
                (crop.size.w - placement.src.size.w).abs() < 1e-6
                    && (crop.size.h - placement.src.size.h).abs() < 1e-6,
                "{mode:?}: {crop:?} != {:?}",
                placement.src
            );
        }
    }

    #[test]
    fn crop_for_window_scales_by_the_buffer_to_output_ratio() {
        // Fill at 2x scale (100x100 buffer onto 200x100 output, cropped):
        // src is 100x50 over a 200x100 destination, so a 50x50 output-space
        // window rect (a quarter of the destination) should map to a 25x25
        // slice of the source.
        let placement = place(WallpaperMode::Fill, buffer(100, 100), output(200, 100));
        let window_rect = Rectangle::<i32, Logical>::new(Point::from((0, 0)), Size::from((50, 50)));
        let crop = crop_for_window(&placement, window_rect);
        assert!((crop.size.w - 25.0).abs() < 1e-6, "{crop:?}");
        assert!((crop.size.h - 25.0).abs() < 1e-6, "{crop:?}");
        assert!((crop.loc.x - placement.src.loc.x).abs() < 1e-6);
        assert!((crop.loc.y - placement.src.loc.y).abs() < 1e-6);
    }

    #[test]
    fn crop_for_window_offsets_by_the_windows_own_location() {
        let placement = place(WallpaperMode::Fit, buffer(100, 100), output(200, 100));
        // Fit centres a 100x100 image inside a 200x100 output at 1:1, so
        // placement.loc = (50, 0). A window at (75, 10) sized 20x20 is 25
        // output-px right of and 10 below the image's own origin.
        let window_rect =
            Rectangle::<i32, Logical>::new(Point::from((75, 10)), Size::from((20, 20)));
        let crop = crop_for_window(&placement, window_rect);
        assert!((crop.loc.x - 25.0).abs() < 1e-6, "{crop:?}");
        assert!((crop.loc.y - 10.0).abs() < 1e-6, "{crop:?}");
        assert!((crop.size.w - 20.0).abs() < 1e-6);
        assert!((crop.size.h - 20.0).abs() < 1e-6);
    }

    // --- decode ----------------------------------------------------------

    #[test]
    fn a_pq_avif_decodes_as_hdr_at_ten_bits() {
        // The whole reason wallpaper decoding lives here rather than in a
        // client: this one CICP field is what makes an HDR wallpaper possible.
        let decoded = decode_file(&fixture("pq16.avif"), 1.0, 0).expect("fixture must decode");
        assert_eq!(decoded.decode, DecodeKind::HdrPq);
        assert_eq!(decoded.fourcc, Fourcc::Xbgr2101010);
        assert_eq!(decoded.size, buffer(16, 16));
        assert_eq!(decoded.frames.len(), 1);
        assert_eq!(decoded.frames[0].pixels.len(), 16 * 16 * 4);
    }

    #[test]
    fn a_non_pq_avif_decodes_as_sdr_at_eight_bits() {
        // Same container, same code path -- only the transfer function differs.
        // An AVIF is not HDR by virtue of being an AVIF.
        let decoded = decode_file(&fixture("sdr16.avif"), 1.0, 0).expect("fixture must decode");
        assert_eq!(decoded.decode, DecodeKind::Sdr);
        assert_eq!(decoded.fourcc, Fourcc::Xbgr8888);
        assert_eq!(decoded.size, buffer(16, 16));
        assert_eq!(decoded.frames[0].pixels.len(), 16 * 16 * 4);
    }

    #[test]
    fn decoded_pixels_are_opaque() {
        // `Wallpaper::new` declares the whole buffer opaque so occlusion
        // culling can skip it. If the decode left real alpha in, that claim
        // would be a lie and windows would composite against garbage.
        let decoded = decode_file(&fixture("sdr16.avif"), 1.0, 0).unwrap();
        assert!(
            decoded.frames[0].pixels.chunks_exact(4).all(|p| p[3] == 0xff),
            "every alpha byte must be 0xff",
        );
    }

    #[test]
    fn an_unreadable_file_reports_why_instead_of_panicking() {
        // These strings reach the user through the diagnostics sink, so they
        // have to name the problem rather than unwinding the compositor.
        // No files are created: the extension is checked before the file is
        // opened, so an unsupported or absent one fails without touching disk.
        assert!(decode_file(&fixture("does-not-exist.avif"), 1.0, 0).is_err());
        assert!(decode_file(&fixture("unsupported.xcf"), 1.0, 0).is_err());
        assert!(decode_file(&fixture("no-extension"), 1.0, 0).is_err());
    }

    // --- assignment ------------------------------------------------------

    #[test]
    fn a_blanket_set_clears_per_output_overrides() {
        // Otherwise "set the wallpaper" silently misses whichever monitor had
        // an override, which reads as a bug rather than as configuration.
        let mut manager = WallpaperManager::default();
        manager.set(Some("DP-3"), &fixture("pq16.avif")).unwrap();
        assert_eq!(
            manager.for_output("DP-3").map(|w| w.decode),
            Some(DecodeKind::HdrPq),
        );
        manager.set(None, &fixture("sdr16.avif")).unwrap();
        assert_eq!(
            manager.for_output("DP-3").map(|w| w.decode),
            Some(DecodeKind::Sdr),
        );
    }

    #[test]
    fn an_output_without_an_override_falls_back_to_the_default() {
        let mut manager = WallpaperManager::default();
        manager.set(None, &fixture("sdr16.avif")).unwrap();
        manager.set(Some("DP-3"), &fixture("pq16.avif")).unwrap();
        assert!(manager.output_has_hdr("DP-3"));
        assert!(!manager.output_has_hdr("HDMI-A-1"));
    }

    #[test]
    fn replacing_a_wallpaper_drops_the_old_image() {
        // A 4K 10-bit image is ~33 MB. Cycling wallpapers must not accumulate.
        let mut manager = WallpaperManager::default();
        manager.set(None, &fixture("pq16.avif")).unwrap();
        manager.set(None, &fixture("sdr16.avif")).unwrap();
        assert_eq!(manager.loaded.len(), 1);
    }

    #[test]
    fn one_image_on_two_outputs_is_decoded_once() {
        let mut manager = WallpaperManager::default();
        manager.set(Some("DP-3"), &fixture("pq16.avif")).unwrap();
        manager.set(Some("HDMI-A-1"), &fixture("pq16.avif")).unwrap();
        assert_eq!(manager.loaded.len(), 1);
    }

    #[test]
    fn a_bad_path_is_reported_and_leaves_the_current_wallpaper_alone() {
        let mut manager = WallpaperManager::default();
        manager.set(None, &fixture("pq16.avif")).unwrap();
        assert!(manager.set(None, Path::new("/nonexistent.avif")).is_err());
        assert!(manager.output_has_hdr("DP-3"), "the good wallpaper must survive");
    }

    #[test]
    fn a_still_wallpaper_never_asks_for_a_repaint() {
        // The animation seam, pinned inert: `advance_all` running every frame
        // must cost nothing and report no change while every image is a still.
        let mut manager = WallpaperManager::default();
        manager.set(None, &fixture("pq16.avif")).unwrap();
        assert!(!manager.advance_all(Instant::now()));
        assert!(!manager.advance_all(Instant::now() + Duration::from_secs(3600)));
    }
}
