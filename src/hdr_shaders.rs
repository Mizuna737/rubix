//! HDR Phase 2 linear-light shaders: the sRGB<->linear texture-shader pair
//! used to composite an `hdr = true` output through a 16-bit-float
//! intermediate (`udev::render_surface`'s HDR branch).
//!
//! Both shaders are `GlesRenderer::compile_custom_texture_shader` programs --
//! same shape as the fork's built-in default texture shader
//! (`backend/renderer/gles/shaders/implicit/texture.frag`), with the
//! sRGB<->linear transfer function folded into the color sample. Compiled
//! once per output (`compile_hdr_shaders`, cached on udev's per-output
//! `SurfaceData::hdr_shaders`) -- never per frame.

use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, UniformName, UniformType,
};
use smithay::backend::renderer::Color32F;

/// BT.2408 reference SDR white, in nits. Fed to `ENCODE_LINEAR_TO_PQ`'s
/// `sdr_white_nits` uniform so the scene-referred [0,1] linear offscreen maps
/// to an absolute PQ luminance. Phase 4 replaces this constant with a live
/// per-output value from the SDR-brightness slider (80-300 nits) -- static
/// 203 nits is correct for now (no slider exists yet).
pub const SDR_WHITE_NITS: f32 = 203.0;

/// Decode pass: samples the client-submitted sRGB texture, converts to
/// linear light (piecewise sRGB EOTF, IEC 61966-2-1: 0.04045 threshold,
/// /12.92 below, `((c+0.055)/1.055)^2.4` above), then multiplies by the
/// element alpha -- mirrors the default texture shader's own alpha handling
/// (`texture.frag`), just with `srgb_to_linear` folded into the color
/// sample. `mix`/`step` (not `bvec` selection -- unavailable in GLSL ES 100)
/// implement the piecewise branch per-channel.
pub const DECODE_SRGB_TO_LINEAR: &str = r#"
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

vec3 srgb_to_linear(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow((c + 0.055) / 1.055, vec3(2.4));
    vec3 cutoff = step(vec3(0.04045), c);
    return mix(lo, hi, cutoff);
}

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(srgb_to_linear(color.rgb), 1.0) * alpha;
#else
    color = vec4(srgb_to_linear(color.rgb), color.a) * alpha;
#endif

    gl_FragColor = color;
}
"#;

/// Encode pass: samples the linear BT.709, scene-referred 16F offscreen,
/// converts to BT.2020 primaries, scales to absolute PQ luminance by
/// `sdr_white_nits`, and applies the SMPTE ST 2084 (PQ) OETF -- the real HDR
/// output encode that supersedes Phase 2's identity sRGB round-trip. All
/// content is still SDR-referred (no HDR clients until Phase 1b), so nothing
/// exceeds panel peak and no tone-mapping is needed this phase -- a straight
/// SDR-white-scaled PQ encode is the whole job.
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
uniform float sdr_white_nits;
varying vec2 v_coords;

// BT.709 -> BT.2020 primaries, linear-light 3x3. GLSL mat3(...) is
// column-major, so this is the transpose of the row-major matrix whose ROWS
// are [0.6274 0.3293 0.0433] / [0.0691 0.9195 0.0114] / [0.0164 0.0880
// 0.8956]. Verified: BT709_TO_BT2020 * vec3(1,1,1) ~= vec3(1,1,1) (each row
// sums to ~1.0), i.e. white maps to white.
const mat3 BT709_TO_BT2020 = mat3(
    0.627403896, 0.069097289, 0.016391439,   // column 0
    0.329283038, 0.919540395, 0.088013308,   // column 1
    0.043313066, 0.011362316, 0.895595253);  // column 2

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
    vec3 lin709 = texture2D(tex, v_coords).rgb;
    vec3 lin2020 = BT709_TO_BT2020 * lin709;
    vec3 y = clamp(lin2020 * sdr_white_nits / 10000.0, 0.0, 1.0);
    gl_FragColor = vec4(pq_oetf(y), 1.0) * alpha;
}
"#;

/// The two compiled HDR texture-shader programs for one output.
/// `GlesTexProgram` is a cheap `Arc` clone (see the fork's
/// `shaders/implicit/mod.rs`), so handing out clones of a cached `HdrShaders`
/// across frames costs nothing -- compilation is the only expensive part,
/// and `compile_hdr_shaders` is called at most once per output (see
/// `SurfaceData::hdr_shaders` in udev.rs).
#[derive(Clone)]
pub struct HdrShaders {
    pub decode: GlesTexProgram,
    pub encode: GlesTexProgram,
}

/// Compile both HDR shaders against the given `GlesRenderer`. Call once per
/// output (on that output's first HDR frame) and cache the result on
/// `SurfaceData::hdr_shaders` -- never per frame.
pub fn compile_hdr_shaders(renderer: &mut GlesRenderer) -> Result<HdrShaders, GlesError> {
    let decode = renderer.compile_custom_texture_shader(DECODE_SRGB_TO_LINEAR, &[])?;
    let encode = renderer.compile_custom_texture_shader(
        ENCODE_LINEAR_TO_PQ,
        &[UniformName::new("sdr_white_nits", UniformType::_1f)],
    )?;
    Ok(HdrShaders { decode, encode })
}

/// Solid-color companion to `DECODE_SRGB_TO_LINEAR`, passed to
/// `GlesRenderer::set_solid_color_transform` during the decode pass so solid
/// colors (the clear color, `SolidColorRenderElement`s) linearize the same
/// way surface textures do. Same piecewise sRGB EOTF constants as the
/// decode shader above, just evaluated on the CPU per draw call instead of
/// per pixel.
pub fn srgb_to_linear_solid(color: Color32F) -> Color32F {
    fn to_linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    Color32F::new(
        to_linear(color.r()),
        to_linear(color.g()),
        to_linear(color.b()),
        color.a(),
    )
}
