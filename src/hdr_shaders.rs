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

use smithay::backend::renderer::gles::{GlesError, GlesRenderer, GlesTexProgram};
use smithay::backend::renderer::Color32F;

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

/// Encode pass: samples the linear 16F offscreen and converts back to sRGB
/// (piecewise, the exact inverse of `DECODE_SRGB_TO_LINEAR` above: 0.0031308
/// linear-domain threshold, `*12.92` below, `1.055*c^(1/2.4)-0.055` above).
/// This is Phase 2's identity round-trip -- the whole point of this shader is
/// that decode-then-encode reproduces the input exactly (mod float
/// precision), so the HDR output looks byte-identical to the non-HDR path.
pub const ENCODE_LINEAR_TO_SRGB: &str = r#"
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

vec3 linear_to_srgb(vec3 c) {
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    vec3 cutoff = step(vec3(0.0031308), c);
    return mix(lo, hi, cutoff);
}

// Phase 3 seam: ENCODE_LINEAR_TO_PQ replaces linear_to_srgb() below with the
// SMPTE ST 2084 PQ OETF plus a nits-scale uniform, once BT.2020 primaries /
// absolute-luminance scanout coordination land (Phase 3/4). Not implemented
// here -- Phase 2 is a pure sRGB round-trip, no nits-scaling yet.

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(linear_to_srgb(color.rgb), 1.0) * alpha;
#else
    color = vec4(linear_to_srgb(color.rgb), color.a) * alpha;
#endif

    gl_FragColor = color;
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
    let encode = renderer.compile_custom_texture_shader(ENCODE_LINEAR_TO_SRGB, &[])?;
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
