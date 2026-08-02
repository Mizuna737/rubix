//! HDR Phase 0 feasibility spike.
//!
//! Standalone, headless, one-shot probe. It does **not** touch the live
//! compositor (`src/udev.rs`, `src/state.rs`, `src/model/grid.rs`,
//! `src/screencopy.rs`) — it builds its own minimal EGL/GLES context on a
//! render node and answers a single question: on this machine's driver
//! stack, can we (a) bind a half-float offscreen render target, (b) run a
//! custom fragment-shader pass into it, and (c) read back genuinely
//! extended-range float precision (not a silent clamp to the 8-bit UNORM
//! ceiling)?
//!
//! See `docs/specs/hdr-phase0-spike.md` for the full spec and
//! `docs/hdr-phase0-findings.md` for the findings this probe writes.
//!
//! Run: `cargo run --example hdr_offscreen_probe`
//! It exits on its own; no mainloop, no lingering GPU process.

use std::ffi::CStr;
use std::fmt::Write as _;
use std::os::raw::c_char;
use std::path::PathBuf;

use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::renderer::gles::{ffi, GlesRenderer, GlesTexture, Uniform, UniformName, UniformType};
use smithay::backend::renderer::{Bind, Frame, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

/// Size of the offscreen probe target. Modest, per spec.
const PROBE_SIZE: i32 = 256;
/// Width of each test band within the probe target.
const BAND_WIDTH: i32 = 64;

/// Extension names we check for half-float renderability, in the order the
/// spec asks for them.
const WATCHED_EXTENSIONS: &[&str] = &[
    "GL_EXT_color_buffer_half_float",
    "GL_OES_texture_half_float",
    "GL_OES_texture_half_float_linear",
    "GL_EXT_color_buffer_float",
];

const HALF_FLOAT_FORMATS: &[Fourcc] = &[Fourcc::Abgr16161616f, Fourcc::Argb16161616f];

/// A trivial custom fragment shader: it writes a uniform `value` into every
/// channel of the target. Enough to prove the custom-shader hook compiles
/// and runs against an offscreen half-float target; the real EOTF/PQ passes
/// will need exactly this hook, just with real math.
const PROBE_FRAGMENT_SHADER: &str = r#"
precision highp float;
uniform float value;
varying vec2 v_coords;
void main() {
    gl_FragColor = vec4(value, value, value, 1.0);
}
"#;

/// Accumulates human-readable findings as the probe runs, so the same log
/// can be printed to stdout and written to `docs/hdr-phase0-findings.md`.
struct Findings {
    lines: Vec<String>,
}

impl Findings {
    fn new() -> Self {
        Findings { lines: Vec::new() }
    }

    fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        println!("{line}");
        self.lines.push(line);
    }
}

/// Decode an IEEE-754 binary16 half float to f32 without pulling in a crate.
fn half_to_f32(half: u16) -> f32 {
    let sign = (half >> 15) & 0x1;
    let exponent = (half >> 10) & 0x1F;
    let mantissa = half & 0x3FF;

    let bits: u32 = if exponent == 0 {
        if mantissa == 0 {
            (sign as u32) << 31
        } else {
            // Subnormal half -> normalized f32.
            let mut exp: i32 = -1;
            let mut mant = mantissa as u32;
            while mant & 0x400 == 0 {
                mant <<= 1;
                exp -= 1;
            }
            mant &= 0x3FF;
            let exp_bits = (exp + 1 + 127 - 15) as u32;
            ((sign as u32) << 31) | (exp_bits << 23) | (mant << 13)
        }
    } else if exponent == 0x1F {
        ((sign as u32) << 31) | (0xFFu32 << 23) | ((mantissa as u32) << 13)
    } else {
        let exp_bits = (exponent as u32) + (127 - 15);
        ((sign as u32) << 31) | (exp_bits << 23) | ((mantissa as u32) << 13)
    };

    f32::from_bits(bits)
}

/// Read back a single RGBA half-float pixel at `(x, y)` from the currently
/// bound framebuffer. Uses raw `glReadPixels` via the frame's GL context
/// rather than Smithay's `ExportMem::map_texture` — that path hardcodes a
/// 4-bytes-per-pixel stride (8-bit assumption) and would truncate half-float
/// (8-bytes-per-pixel) data. This is exactly the read-back gap the spec's
/// risk section anticipated.
fn read_half_float_pixel(
    frame: &mut smithay::backend::renderer::gles::GlesFrame<'_, '_>,
    x: i32,
    y: i32,
) -> Result<[f32; 4], String> {
    let mut raw = [0u16; 4];
    frame
        .with_context(|gl| unsafe {
            gl.ReadPixels(
                x,
                y,
                1,
                1,
                ffi::RGBA,
                ffi::HALF_FLOAT,
                raw.as_mut_ptr() as *mut _,
            );
            gl.GetError()
        })
        .map_err(|e| format!("with_context failed: {e:?}"))
        .and_then(|gl_err| {
            if gl_err == ffi::NO_ERROR {
                Ok(())
            } else {
                Err(format!("glReadPixels failed with GL error 0x{gl_err:x}"))
            }
        })?;

    Ok([
        half_to_f32(raw[0]),
        half_to_f32(raw[1]),
        half_to_f32(raw[2]),
        half_to_f32(raw[3]),
    ])
}

/// Find a render node to build the probe's EGL context on. Prefers
/// `/dev/dri/renderD128`, falls back to whatever `EGLDevice::enumerate()`
/// reports. Never touches a device by any path that could imply DRM master
/// (this probe never opens a DRM device node directly, only goes through
/// `EGLDevice`, which only needs render access).
fn find_render_device(findings: &mut Findings) -> Result<EGLDevice, String> {
    let devices: Vec<EGLDevice> =
        EGLDevice::enumerate().map_err(|e| format!("EGLDevice::enumerate failed: {e:?}"))?.collect();

    if devices.is_empty() {
        return Err("no EGL devices found".to_string());
    }

    let preferred = PathBuf::from("/dev/dri/renderD128");
    if let Some(device) = devices
        .iter()
        .find(|d| d.render_device_path().ok().as_ref() == Some(&preferred))
    {
        findings.log(format!("render node: {} (preferred)", preferred.display()));
        return Ok(device.clone());
    }

    if let Some(device) = devices.iter().find(|d| d.render_device_path().is_ok()) {
        let path = device.render_device_path().unwrap();
        findings.log(format!(
            "render node: {} (fallback, {} was unavailable)",
            path.display(),
            preferred.display()
        ));
        return Ok(device.clone());
    }

    Err("no EGL device exposed a render node path".to_string())
}

/// Result of the decisive step 4 read-back test.
struct ReadbackResult {
    high_value: f32,
    precision_a: f32,
    precision_b: f32,
}

fn run_probe(findings: &mut Findings) -> Result<bool, String> {
    // --- context setup ---------------------------------------------------
    let device = find_render_device(findings)?;
    let display =
        unsafe { EGLDisplay::new(device).map_err(|e| format!("EGLDisplay::new failed: {e:?}"))? };
    let context =
        EGLContext::new(&display).map_err(|e| format!("EGLContext::new failed: {e:?}"))?;
    let mut renderer =
        unsafe { GlesRenderer::new(context).map_err(|e| format!("GlesRenderer::new failed: {e:?}"))? };

    // --- step 1: context/extension probe ----------------------------------
    let (version_string, extensions) = renderer
        .with_context(|gl| unsafe {
            let version_ptr = gl.GetString(ffi::VERSION) as *const c_char;
            let version = if version_ptr.is_null() {
                "<unknown>".to_string()
            } else {
                CStr::from_ptr(version_ptr).to_string_lossy().into_owned()
            };
            let ext_ptr = gl.GetString(ffi::EXTENSIONS) as *const c_char;
            let extensions: Vec<String> = if ext_ptr.is_null() {
                Vec::new()
            } else {
                CStr::from_ptr(ext_ptr)
                    .to_string_lossy()
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect()
            };
            (version, extensions)
        })
        .map_err(|e| format!("querying GL version/extensions failed: {e:?}"))?;

    findings.log(format!("GLES version string: {version_string}"));
    findings.log("extension table:".to_string());
    for name in WATCHED_EXTENSIONS {
        let present = extensions.iter().any(|e| e == name);
        findings.log(format!("  {name}: {}", if present { "present" } else { "absent" }));
    }

    // --- step 2: half-float offscreen bind --------------------------------
    let size = Size::<i32, BufferCoord>::from((PROBE_SIZE, PROBE_SIZE));
    let mut bound_format = None;
    let mut texture: Option<GlesTexture> = None;
    let mut bind_errors = Vec::new();

    for fourcc in HALF_FLOAT_FORMATS {
        match Offscreen::<GlesTexture>::create_buffer(&mut renderer, *fourcc, size) {
            Ok(tex) => {
                bound_format = Some(*fourcc);
                texture = Some(tex);
                break;
            }
            Err(e) => bind_errors.push(format!("{fourcc:?}: {e:?}")),
        }
    }

    let Some(bound_format) = bound_format else {
        findings.log(format!(
            "half-float offscreen bind: FAILED for all candidate formats ({})",
            bind_errors.join("; ")
        ));
        findings.log("verdict reason: no half-float format could be bound");
        return Ok(false);
    };
    findings.log(format!("half-float offscreen bind: bound {bound_format:?}"));

    let mut texture = texture.expect("checked Some above");
    let mut target = renderer
        .bind(&mut texture)
        .map_err(|e| format!("Bind::bind failed: {e:?}"))?;

    // --- step 3: custom shader compile ------------------------------------
    let program = match renderer.compile_custom_pixel_shader(
        PROBE_FRAGMENT_SHADER,
        &[UniformName::new("value", UniformType::_1f)],
    ) {
        Ok(program) => {
            findings.log("custom pixel shader: compiled OK".to_string());
            program
        }
        Err(e) => {
            // The GL shader/program info-log (if any) is emitted by Smithay
            // itself via `tracing::error!` inside `link_program` /
            // `compile_shader` — visible above this line if RUST_LOG is set.
            findings.log(format!("custom pixel shader: FAILED to compile ({e:?})"));
            findings.log("verdict reason: custom pixel shader failed to compile");
            return Ok(false);
        }
    };

    // --- steps 3+4: run the shader, read back three bands -----------------
    let mut frame = renderer
        .render(&mut target, Size::from((PROBE_SIZE, PROBE_SIZE)), Transform::Normal)
        .map_err(|e| format!("Renderer::render failed: {e:?}"))?;

    // Band A: the decisive extended-range test (well above the 1.0 UNORM ceiling).
    // Band B/C: two values under 1/255 apart, to check sub-8-bit precision survives.
    let bands: [(i32, f32); 3] = [(0, 4.0), (BAND_WIDTH, 0.5000), (2 * BAND_WIDTH, 0.5020)];
    let mut shader_ran = true;
    for (band_x, value) in bands {
        let dest = Rectangle::<i32, Physical>::new((band_x, 0).into(), (BAND_WIDTH, PROBE_SIZE).into());
        let src = Rectangle::<f64, BufferCoord>::from_size((BAND_WIDTH as f64, PROBE_SIZE as f64).into());
        let band_size = Size::<i32, BufferCoord>::from((BAND_WIDTH, PROBE_SIZE));
        if let Err(e) = frame.render_pixel_shader_to(
            &program,
            src,
            dest,
            band_size,
            None,
            1.0,
            &[Uniform::new("value", value)],
        ) {
            findings.log(format!("custom pixel shader: pass at x={band_x} FAILED to run ({e:?})"));
            shader_ran = false;
        }
    }
    if shader_ran {
        findings.log("custom pixel shader: all passes executed".to_string());
    } else {
        findings.log("verdict reason: custom pixel shader pass did not execute");
        return Ok(false);
    }

    // Read back before finishing the frame -- the FBO stays bound for the
    // frame's whole lifetime (Smithay binds it once in `Renderer::render`),
    // so this is the same framebuffer the shader just wrote into.
    let readback = (|| -> Result<ReadbackResult, String> {
        let high = read_half_float_pixel(&mut frame, BAND_WIDTH / 2, PROBE_SIZE / 2)?;
        let a = read_half_float_pixel(&mut frame, BAND_WIDTH + BAND_WIDTH / 2, PROBE_SIZE / 2)?;
        let b = read_half_float_pixel(&mut frame, 2 * BAND_WIDTH + BAND_WIDTH / 2, PROBE_SIZE / 2)?;
        Ok(ReadbackResult {
            high_value: high[0],
            precision_a: a[0],
            precision_b: b[0],
        })
    })();

    frame
        .finish()
        .map_err(|e| format!("Frame::finish failed: {e:?}"))?
        .wait()
        .map_err(|e| format!("SyncPoint::wait failed: {e:?}"))?;

    let readback = match readback {
        Ok(r) => r,
        Err(e) => {
            findings.log(format!("read-back: FAILED ({e})"));
            findings.log("verdict reason: glReadPixels read-back failed");
            return Ok(false);
        }
    };

    findings.log(format!(
        "read-back (extended-range test): wrote 4.0, read back {:.6}",
        readback.high_value
    ));
    findings.log(format!(
        "read-back (sub-LSB precision test): wrote 0.5000 / 0.5020, read back {:.6} / {:.6}",
        readback.precision_a, readback.precision_b
    ));

    let extended_range_preserved = (readback.high_value - 4.0).abs() < 0.05;
    let precision_distinct = (readback.precision_a - readback.precision_b).abs() > 0.0005;

    if !extended_range_preserved {
        findings.log(format!(
            "verdict reason: 4.0 write clamped on read-back to {:.6} -- target is not genuinely extended-range",
            readback.high_value
        ));
        return Ok(false);
    }
    if !precision_distinct {
        findings.log(format!(
            "verdict reason: sub-8-bit-LSB values {:.6}/{:.6} did not read back distinct",
            readback.precision_a, readback.precision_b
        ));
        return Ok(false);
    }

    Ok(true)
}

fn write_findings_doc(findings: &Findings, verdict: bool) {
    let mut doc = String::new();
    let _ = writeln!(doc, "# HDR Phase 0 findings\n");
    let _ = writeln!(
        doc,
        "Generated by `examples/hdr_offscreen_probe.rs`. See `docs/specs/hdr-phase0-spike.md` for the spec.\n"
    );
    let _ = writeln!(doc, "## Probe log\n");
    let _ = writeln!(doc, "```");
    for line in &findings.lines {
        let _ = writeln!(doc, "{line}");
    }
    let _ = writeln!(
        doc,
        "HDR_PHASE0_VERDICT: {}",
        if verdict { "GO" } else { "NO-GO" }
    );
    let _ = writeln!(doc, "```\n");
    let _ = writeln!(
        doc,
        "## Verdict\n\n**{}**\n",
        if verdict { "GO" } else { "NO-GO" }
    );

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/hdr-phase0-findings.md");
    if let Err(e) = std::fs::write(path, doc) {
        eprintln!("hdr_offscreen_probe: failed to write findings doc: {e}");
    }
}

fn main() {
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt::init();
    }

    let mut findings = Findings::new();
    let verdict = match run_probe(&mut findings) {
        Ok(verdict) => verdict,
        Err(e) => {
            findings.log(format!("probe aborted: {e}"));
            findings.log(format!("verdict reason: {e}"));
            false
        }
    };

    write_findings_doc(&findings, verdict);

    // The single required verdict line, printed exactly once, last.
    println!("HDR_PHASE0_VERDICT: {}", if verdict { "GO" } else { "NO-GO" });
}
