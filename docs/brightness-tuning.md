# Brightness tuning

Five knobs control how bright things look. Each one owns a different part of
the screen, and they are deliberately independent — retuning one does not drag
another with it.

If you just want to know which dial to turn, the table in
`config/default.toml` next to each key is enough. This document is the
long-form version: what each knob measures against, and why the awkward ones
are awkward.

| Knob | Affects | Up means |
|---|---|---|
| `sdr_white_nits` | ordinary windows, HDR output only | brighter windows |
| `wallpaper.luminance_scale` | wallpaper, HDR output only | brighter wallpaper |
| `wallpaper.sdr_reference_nits` | wallpaper, **non-HDR** output only | **darker** wallpaper |
| `decoration.active_luminance_nits` | focused window's border, HDR output only | brighter focus ring |
| `decoration.backdrop_luminance_nits` | wallpaper seen *through* a window | more wallpaper shows through |

## Why there are two wallpaper knobs

The two display paths measure against different things, so one number cannot
serve both.

On an HDR output the image is shown at its **absolute** graded luminance —
whatever the file says, the panel emits. On an SDR output there is no absolute
luminance to hit, so the image has to be normalised against some **reference
white**, and the BT.2408 default of 203 nits assumes the content was graded to
that reference.

SDR-to-HDR converted images usually are not. They lift the *whole* image
rather than only extending the highlights, so the same file that looks right in
HDR comes out far too bright in SDR, with the shadows lifted worst of all.

- `luminance_scale` is a linear-light gain applied at decode. It pulls an
  over-lifted conversion back down while leaving the highlight headroom intact.
  Changing it re-decodes the image.
- `sdr_reference_nits` says what luminance in the source counts as white on an
  SDR output. It is measured against the file's **own grading**, before
  `luminance_scale` is applied, which is what keeps the two orthogonal: the
  gain multiplies through and cancels, so retuning HDR brightness does not
  quietly push the SDR output the other way.

### Why raising `sdr_reference_nits` makes the image darker

It is a reference *white*, not a brightness. Declaring that 3000 nits counts as
white means a 300-nit pixel is only a tenth of white, so it renders dark.
Declaring that 300 nits counts as white makes that same pixel full white.
Higher reference, dimmer image.

When tuning it, match the shadows against an SDR original first — a lifted
black reads as "washed out" far more readily than a dimmed highlight does.

## Why the focus ring has its own knob

`active_luminance_nits` exists to put chrome *above* SDR white. With
`sdr_white_nits` at 203, a 350-nit focus ring is a focus indication that SDR
chrome physically cannot express.

This is also what makes the glow work. In SDR a glow has to brighten toward
white to be visible at all, which washes out its hue and competes with window
content — which is why glows tend to look gaudy. With luminance above SDR white
the glow keeps its saturation and simply sits brighter than the desktop,
reading as light spilling off the window edge. Subtlety comes from the
headroom, not from lowering the opacity.

## Why the backdrop needs a ceiling at all

The compositor composites in absolute linear light, so window `opacity` is a
true physical transmittance: at 0.95 a window passes 5% of whatever is behind
it *in nits*. Behind an HDR wallpaper peaking near 460 nits, that is ~23 nits
landing on a window background of ~1.6 — so the wallpaper reads clearly through
the glass and text contrast collapses.

The same window over another window looks fine, because another SDR window is
two orders of magnitude dimmer than an HDR one. That asymmetry is the tell.

`backdrop_luminance_nits` caps what may bleed through. It is measured in
**absolute display nits** — the luminance a backdrop may actually reach on
screen — which is what makes it directly comparable to `sdr_white_nits`. Unlike
`sdr_reference_nits` it is *not* scaled by `luminance_scale`, because the gain
is already baked into the decoded texels.

### The trap

The roll-off knee sits at 0.8x this value. Set it above the wallpaper's actual
peak luminance and the cap becomes an exact no-op — toggling tonemap then
changes literally nothing, which is indistinguishable from the feature not
being wired up at all. If tonemap appears to do nothing, check this number
against your wallpaper's peak before assuming anything is broken.

Useful values sit well *below* `sdr_white_nits`. Around 40 is a reasonable
starting point for a backdrop that recedes behind text.

### Blur interacts with it

Blurring happens in linear light, which raises the local mean wherever a
specular highlight sits — a frosted pane reads *brighter* than the sharp
wallpaper around it, not dimmer, because averaging in nits lets one bright
highlight drag its neighbours up. `backdrop_luminance_nits` is what keeps that
brightened result under window text luminance, so pair blur with tonemap rather
than using blur alone on a bright HDR wallpaper.

The roll-off is driven by the max component and applied as a single scalar
factor to all three channels, so it compresses intensity without touching
chromaticity. Rolling each channel independently would desaturate every bright
colour toward white.
