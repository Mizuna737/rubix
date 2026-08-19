//! HDR Phase 1b color-management shaders: the per-surface decode/encode
//! texture-shader trio used to composite an `hdr = true` output through a
//! 16-bit-float intermediate (`udev::render_surface`'s HDR branch).
//!
//! Working space of the 16F offscreen is linear BT.2020, absolute luminance
//! normalized to 10000 nits (`1.0` == 10000 cd/m², PQ-linear). SDR surfaces
//! decode via `DECODE_SDR` (sRGB EOTF -> BT.709->BT.2020 -> nits scaling);
//! HDR PQ surfaces decode via `DECODE_HDR_PQ` (ST 2084 inverse EOTF,
//! BT.2020 passthrough). Both land in the same working space, so the encode
//! shader collapses to a bare PQ OETF.
//!
//! Each is a `GlesRenderer::compile_custom_texture_shader` program -- same
//! shape as the fork's built-in default texture shader
//! (`backend/renderer/gles/shaders/implicit/texture.frag`). Compiled once
//! per output (`compile_hdr_shaders`, cached on udev's per-output
//! `SurfaceData::hdr_shaders`) -- never per frame.

use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, UniformName, UniformType,
};
use smithay::backend::renderer::Color32F;

/// BT.2408 reference SDR white, in nits. Historically fed straight to the
/// encode shader's `sdr_white_nits` uniform; as of Phase 4 the SDR decode
/// pass instead reads the live, runtime-adjustable `RubixState::sdr_white_nits`
/// (see udev.rs::render_surface_hdr), which starts out seeded from this
/// constant. This const now serves two remaining purposes: the serde default
/// for `Config::sdr_white_nits` (`config::default_sdr_white_nits`) and the
/// documented center of its valid range, [80, 300] nits -- the clamp bounds
/// enforced everywhere the value can change (config resolve, hot-reload, and
/// the IncreaseSdrWhite/DecreaseSdrWhite keybind actions).
pub const SDR_WHITE_NITS: f32 = 203.0;

/// SDR decode pass: samples the client-submitted sRGB texture, converts to
/// linear light (piecewise sRGB EOTF, IEC 61966-2-1: 0.04045 threshold,
/// /12.92 below, `((c+0.055)/1.055)^2.4` above), reprojects BT.709 -> BT.2020
/// primaries, and scales by `sdr_white_nits/10000` to land in the shared
/// absolute-luminance working space -- then multiplies by the element alpha,
/// mirroring the default texture shader's own alpha handling (`texture.frag`).
/// `mix`/`step` (not `bvec` selection -- unavailable in GLSL ES 100) implement
/// the piecewise sRGB branch per-channel.
pub const DECODE_SDR: &str = r#"
#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
uniform float sdr_white_nits;
varying vec2 v_coords;

vec3 srgb_to_linear(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow((c + 0.055) / 1.055, vec3(2.4));
    vec3 cutoff = step(vec3(0.04045), c);
    return mix(lo, hi, cutoff);
}

// BT.709 -> BT.2020 primaries, linear-light 3x3. GLSL mat3(...) is
// column-major, so this is the transpose of the row-major matrix whose ROWS
// are [0.6274 0.3293 0.0433] / [0.0691 0.9195 0.0114] / [0.0164 0.0880
// 0.8956]. Verified: BT709_TO_BT2020 * vec3(1,1,1) ~= vec3(1,1,1) (each row
// sums to ~1.0), i.e. white maps to white.
const mat3 BT709_TO_BT2020 = mat3(
    0.627403896, 0.069097289, 0.016391439,   // column 0
    0.329283038, 0.919540395, 0.088013308,   // column 1
    0.043313066, 0.011362316, 0.895595253);  // column 2

void main() {
    vec4 c = texture2D(tex, v_coords);
    vec3 lin709 = srgb_to_linear(c.rgb);
    vec3 lin2020 = BT709_TO_BT2020 * lin709;
    vec3 abs10k = lin2020 * (sdr_white_nits / 10000.0);

#if defined(NO_ALPHA)
    gl_FragColor = vec4(abs10k, 1.0) * alpha;
#else
    gl_FragColor = vec4(abs10k, c.a) * alpha;
#endif
}
"#;

/// HDR decode pass: ST 2084 (PQ) inverse EOTF, BT.2020 passthrough. Input is
/// PQ-encoded BT.2020 (assumed -- PQ content is BT.2020 in practice, no
/// client-declared-primaries matrix needed per this phase's scope); output is
/// linear BT.2020, absolute luminance normalized to 10000 nits -- landing
/// directly in the shared working space with no scaling. Opaque video, but
/// `* alpha` is kept for parity with the other texture-shader programs. No
/// custom uniform beyond the standard `alpha`.
pub const DECODE_HDR_PQ: &str = r#"
#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

// SMPTE ST 2084 / Rec. 2100 PQ inverse EOTF (decode), m1 m2 c1 c2 c3 -- same
// constants as the encode shader's `pq_oetf`, inverted.
vec3 pq_eotf(vec3 e) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 ep = pow(e, vec3(1.0 / m2));
    vec3 num = max(ep - c1, 0.0);
    vec3 den = c2 - c3 * ep;
    return pow(num / den, vec3(1.0 / m1));   // 0..1 == 0..10000 nits
}

void main() {
    vec4 c = texture2D(tex, v_coords);
    gl_FragColor = vec4(pq_eotf(c.rgb), 1.0) * alpha;
}
"#;

/// Windows-scRGB decode pass: BT.709 primaries, **already linear**, extended
/// range, into the shared linear BT.2020 / 10000-nit working space.
///
/// This is the encoding Windows 10 defines for an HDR screen driven in
/// BT.2100/PQ mode, and it is what DXGI titles produce through
/// `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`. Three things make it unlike
/// [`DECODE_SDR`] despite sharing its primaries:
///
///  - **No EOTF.** The transfer characteristic is extended *linear*. Running
///    `srgb_to_linear` here would darken everything by roughly a 2.2 power.
///  - **1.0 is 80 cd/m², not the SDR white level.** Fixed by the protocol, so
///    unlike `DECODE_SDR` this takes no `sdr_white_nits` uniform -- the slider
///    must not move HDR content. 125.0 is the maximum and lands exactly on
///    10000 cd/m², which is why the scale below saturates the working space at
///    precisely the right place.
///  - **Values may be negative**, deliberately: that is how the encoding
///    escapes the sRGB gamut boundary. Negatives survive the matrix (it is
///    linear), and are clipped only after it, in BT.2020 -- a much wider gamut,
///    so most sRGB-negative colors land positive and are preserved rather than
///    crushed. The clip matters because the PQ OETF downstream raises its input
///    to a fractional power and would produce NaN on a negative.
pub const DECODE_WINDOWS_SCRGB: &str = r#"
#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

// Identical column-major BT.709 -> BT.2020 matrix as DECODE_SDR.
const mat3 BT709_TO_BT2020 = mat3(
    0.627403896, 0.069097289, 0.016391439,   // column 0
    0.329283038, 0.919540395, 0.088013308,   // column 1
    0.043313066, 0.011362316, 0.895595253);  // column 2

// Windows-scRGB: nominal 1.0 == 80 cd/m^2, 125.0 == 10000 cd/m^2. The working
// space is normalized so 1.0 == 10000 cd/m^2.
const float SCRGB_WHITE_NITS = 80.0;

void main() {
    vec4 c = texture2D(tex, v_coords);
    // NO EOTF: the input is already linear light.
    vec3 lin2020 = BT709_TO_BT2020 * c.rgb;
    // Clip out-of-BT.2020 negatives; the PQ OETF cannot take them.
    vec3 clipped = max(lin2020, 0.0);
    vec3 abs10k = min(clipped * (SCRGB_WHITE_NITS / 10000.0), 1.0);

#if defined(NO_ALPHA)
    gl_FragColor = vec4(abs10k, 1.0) * alpha;
#else
    gl_FragColor = vec4(abs10k, c.a) * alpha;
#endif
}
"#;

/// Encode pass: samples the linear BT.2020, absolute-luminance (normalized to
/// 10000 nits) 16F offscreen and applies the bare SMPTE ST 2084 (PQ) OETF --
/// the real HDR output encode. The BT.709->BT.2020 matrix and
/// `sdr_white_nits` scaling moved OUT of this shader and INTO `DECODE_SDR`
/// (Phase 1b): every surface, SDR or HDR, already lands in this space by the
/// time the encode pass runs, so encode is just the OETF.
pub const ENCODE_LINEAR_TO_PQ: &str = r#"
#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

// SMPTE ST 2084 / Rec. 2100 PQ OETF, m1 m2 c1 c2 c3.
vec3 pq_oetf(vec3 y) {
    const float m1 = 0.1593017578125;   // 2610/16384
    const float m2 = 78.84375;          // 2523/4096 * 128
    const float c1 = 0.8359375;         // 3424/4096
    const float c2 = 18.8515625;        // 2413/4096 * 32
    const float c3 = 18.6875;           // 2392/4096 * 32
    vec3 ym = pow(y, vec3(m1));
    return pow((c1 + c2 * ym) / (1.0 + c3 * ym), vec3(m2));
}

void main() {
    vec3 lin = clamp(texture2D(tex, v_coords).rgb, 0.0, 1.0);
    gl_FragColor = vec4(pq_oetf(lin), 1.0) * alpha;
}
"#;

/// The three compiled HDR texture-shader programs for one output.
/// `GlesTexProgram` is a cheap `Arc` clone (see the fork's
/// `shaders/implicit/mod.rs`), so handing out clones of a cached `HdrShaders`
/// across frames costs nothing -- compilation is the only expensive part,
/// and `compile_hdr_shaders` is called at most once per output (see
/// `SurfaceData::hdr_shaders` in udev.rs).
#[derive(Clone)]
pub struct HdrShaders {
    pub decode_sdr: GlesTexProgram,
    pub decode_hdr_pq: GlesTexProgram,
    /// Windows-scRGB (linear BT.709, 1.0 == 80 nits). See
    /// [`DECODE_WINDOWS_SCRGB`].
    pub decode_windows_scrgb: GlesTexProgram,
    pub encode: GlesTexProgram,
}

/// Compile all HDR shaders against the given `GlesRenderer`. Call once
/// per output (on that output's first HDR frame) and cache the result on
/// `SurfaceData::hdr_shaders` -- never per frame.
pub fn compile_hdr_shaders(renderer: &mut GlesRenderer) -> Result<HdrShaders, GlesError> {
    let decode_sdr = renderer.compile_custom_texture_shader(
        DECODE_SDR,
        &[UniformName::new("sdr_white_nits", UniformType::_1f)],
    )?;
    let decode_hdr_pq = renderer.compile_custom_texture_shader(DECODE_HDR_PQ, &[])?;
    let decode_windows_scrgb =
        renderer.compile_custom_texture_shader(DECODE_WINDOWS_SCRGB, &[])?;
    let encode = renderer.compile_custom_texture_shader(ENCODE_LINEAR_TO_PQ, &[])?;
    Ok(HdrShaders {
        decode_sdr,
        decode_hdr_pq,
        decode_windows_scrgb,
        encode,
    })
}

/// CPU-side sRGB EOTF + BT.709->BT.2020 + nits scaling, factored out of
/// [`sdr_solid_transform`] so it's independently unit-testable.
fn srgb_to_bt2020_abs10k(color: Color32F, sdr_white_nits: f32) -> Color32F {
    fn to_linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    let lin709 = [to_linear(color.r()), to_linear(color.g()), to_linear(color.b())];
    // Same column-major BT.709->BT.2020 matrix as `DECODE_SDR`'s GLSL, applied
    // by hand on the CPU (columns 0/1/2 as commented there).
    const M: [[f32; 3]; 3] = [
        [0.627403896, 0.069097289, 0.016391439],
        [0.329283038, 0.919540395, 0.088013308],
        [0.043313066, 0.011362316, 0.895595253],
    ];
    let mut lin2020 = [0.0f32; 3];
    for row in 0..3 {
        lin2020[row] = M[0][row] * lin709[0] + M[1][row] * lin709[1] + M[2][row] * lin709[2];
    }
    let scale = sdr_white_nits / 10000.0;
    Color32F::new(
        lin2020[0] * scale,
        lin2020[1] * scale,
        lin2020[2] * scale,
        color.a(),
    )
}

/// Solid-color companion to `DECODE_SDR`, passed to
/// `GlesRenderer::set_solid_color_transform` during the SDR decode override
/// so solid colors (the clear color, `SolidColorRenderElement`s) linearize
/// and land in the shared working space the same way SDR surface textures
/// do. Solids are never HDR, so this is the only solid transform the decode
/// pass ever needs -- set once for the whole pass, covering every run.
/// Captures the live `sdr_white_nits` value (Phase 1b moved the nits scaling
/// out of the encode shader and into here + `DECODE_SDR`).
pub fn sdr_solid_transform(sdr_white_nits: f32) -> impl Fn(Color32F) -> Color32F + 'static {
    move |color| srgb_to_bt2020_abs10k(color, sdr_white_nits)
}

#[cfg(test)]
mod scrgb_tests {
    use super::*;

    // The whole point of this shader is that Windows-scRGB is ALREADY LINEAR.
    // It shares primaries with DECODE_SDR, so the obvious way to write it is to
    // copy that shader and change the scale -- which silently leaves the sRGB
    // EOTF in and darkens everything by roughly a 2.2 power. Nothing downstream
    // would error; the picture would just be wrong, which is exactly the class
    // of bug that cost us this whole investigation.
    #[test]
    fn scrgb_decode_applies_no_eotf() {
        assert!(
            !DECODE_WINDOWS_SCRGB.contains("srgb_to_linear"),
            "Windows-scRGB is extended LINEAR; applying an EOTF darkens all HDR content"
        );
        assert!(
            !DECODE_WINDOWS_SCRGB.contains("pq_eotf"),
            "Windows-scRGB is not PQ"
        );
    }

    // Protocol-fixed anchors: 1.0 -> 80 cd/m², 125.0 -> 10000 cd/m². The
    // working space normalizes 1.0 to 10000 cd/m², so the scale must be
    // 80/10000 and 125.0 must land exactly at the top of the range.
    #[test]
    fn scrgb_white_level_is_the_protocol_fixed_80_nits() {
        assert!(
            DECODE_WINDOWS_SCRGB.contains("SCRGB_WHITE_NITS = 80.0"),
            "Windows-scRGB pins 1.0 to 80 cd/m² by protocol"
        );
        let scale = 80.0f32 / 10000.0;
        assert!((125.0f32 * scale - 1.0).abs() < 1e-6, "125.0 must map to 10000 nits");
        assert!((1.0f32 * scale - 0.008).abs() < 1e-6, "1.0 must map to 80 nits");
    }

    // The SDR brightness slider must not move HDR content: scRGB's white level
    // is fixed by the protocol, so this shader takes no sdr_white_nits uniform.
    #[test]
    fn scrgb_decode_takes_no_sdr_white_uniform() {
        assert!(
            !DECODE_WINDOWS_SCRGB.contains("sdr_white_nits"),
            "the SDR slider must not scale protocol-fixed HDR content"
        );
        assert!(DECODE_SDR.contains("sdr_white_nits"), "but SDR content still follows it");
    }

    // Negatives are how scRGB escapes the sRGB gamut, so they must survive the
    // matrix and be clipped only afterwards, in the much wider BT.2020 space.
    #[test]
    fn scrgb_decode_clips_after_the_matrix_not_before() {
        let matrix = DECODE_WINDOWS_SCRGB.find("BT709_TO_BT2020 * c.rgb").expect("matrix applied");
        let clip = DECODE_WINDOWS_SCRGB.find("max(lin2020, 0.0)").expect("negatives clipped");
        assert!(clip > matrix, "clipping before the matrix crushes in-BT.2020 colors");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f32 = 1e-4;

    #[test]
    fn white_maps_to_nits_over_10k() {
        let nits = 203.0_f32;
        let out = srgb_to_bt2020_abs10k(Color32F::new(1.0, 1.0, 1.0, 1.0), nits);
        let expected = nits / 10000.0;
        assert!((out.r() - expected).abs() < EPSILON, "r={}", out.r());
        assert!((out.g() - expected).abs() < EPSILON, "g={}", out.g());
        assert!((out.b() - expected).abs() < EPSILON, "b={}", out.b());
        assert!((out.a() - 1.0).abs() < EPSILON);
    }

    #[test]
    fn black_maps_to_zero() {
        let out = srgb_to_bt2020_abs10k(Color32F::new(0.0, 0.0, 0.0, 1.0), 203.0);
        assert!(out.r().abs() < EPSILON);
        assert!(out.g().abs() < EPSILON);
        assert!(out.b().abs() < EPSILON);
    }
}
