# Wallpaper-derived theming

Rubix extracts a colour palette from the wallpaper it is already displaying,
solves a theme whose text is guaranteed readable through window transparency,
and hands the result to the rest of the desktop.

## Why it lives in the compositor

Every palette generator in the wild — pywal, wallust, matugen — takes an 8-bit
sRGB image from a file path. Handing one a PQ / BT.2020 AVIF yields a palette
read through the wrong transfer function: the picture is not dark, but its
*encoding* looks dark to anything assuming sRGB, so the colours come out
crushed and desaturated. As of this writing no tool anywhere extracts palettes
from HDR images.

The compositor has already resolved the transfer function from the file's CICP
block, so extraction starts from pixels whose luminance is *known*.

The second reason is contrast. Windows are translucent over a refracted
wallpaper, so what sits behind a glyph is

```
background * opacity + backdrop * (1 - opacity)
```

No application can reason about that — Kitty does not know what is behind
Kitty. The compositor holds both the text colour and what it will be
composited over, which makes contrast a compositor-shaped problem.

## Extraction (`src/palette.rs`)

Pixels are strided down to ~40k samples (fixed stride, so the same wallpaper
always yields the same palette) and clustered with k-means in **ICtCp**.

ICtCp (Rec. BT.2100) applies the PQ curve — the one already encoding these
files — to an LMS cone response. It is perceptually uniform in *absolute*
luminance across 0–10000 cd/m², over BT.2020 primaries. CIELAB and Oklab both
normalise to a diffuse white, which collapses a 4000-nit highlight and a
200-nit sky into the same "white": exactly the distinction an HDR wallpaper
exists to make.

`kmeans_colors` is used with `default-features = false`, reducing it to a
generic k-means over `rand` — both already in the tree, so it costs one new
package. It is used for its `Calculate` trait, implemented on a caller-owned
ICtCp point, which is what keeps clustering in absolute terms.

SDR wallpapers reach the same space through `hdr_shaders::srgb_to_bt2020_abs10k`,
the CPU twin of the SDR decode shader, so an sRGB PNG yields a palette derived
from precisely the pixels the renderer puts on screen.

## Solving (`src/theme.rs`)

Colours are **solved**, not picked. Foreground intensity is binary-searched to
the dimmest value clearing an **APCA** target against the composite. APCA (the
WCAG 3 draft model) is polarity-aware; WCAG 2's symmetric ratio systematically
overstates light-on-dark legibility, which is this theme's whole case.

When a wallpaper cannot support the target, `met_targets` reports the shortfall
rather than silently returning white.

### Which background counts as "worst"

Not the average — a theme keyed to the mean fails everywhere the wallpaper is
brighter than its mean, which is half of it. The solve uses a high percentile,
chosen by how the backdrop is composited:

- **blurred** backdrop → `p95`; blur has already averaged small bright specks away
- **sharp** backdrop → `p99`; it has not, so a small highlight survives intact

`p95` is blind to a bright region covering less than a twentieth of the image;
a test pins that limitation rather than leaving it to be rediscovered.

Solve parameters derive from the live `[decoration]` styles, taking the worst
case across active and inactive — but see the warning under **Borders** below.

### Roles

`background`, `surface`, `foreground`, `muted`, `accent`, `border`, `glow`, and
six `ansi` entries. The ANSI hues are anchored to references so red stays red,
then rotated toward the wallpaper's accent by a capped amount — a pure
fractional pull let a distant accent drag red into magenta and collapse it onto
the magenta slot, so there is a hard ~14° ceiling as well.

### Borders

`glow` (focused) and `border` (unfocused) come from a swatch **actually present
in the wallpaper**, skipping the most populous one — that is the wallpaper's own
background, which a border cannot stand out against. A synthesized complement
stands out but reads as an unrelated colour laid on top, because nothing else on
screen is that hue.

Two failure modes, both found by running the whole 97-image library and both
regression-tested:

- **Near-black borders.** Saturation and luminance trade against each other at
  the gamut edge: a hue sRGB can barely represent stays in gamut at high chroma
  *only while very dark*. Maximising chroma first picked the dark end every
  time, and no luminance setting rescues a black colour. Brightness is the
  requirement; chroma is spent to reach it.
- **Clipped primaries.** A neon wallpaper's chroma sits outside sRGB, so solving
  intensity drove two channels past 1.0 and one below 0 and the clamp produced
  `#ff00ff`. The accent now backs off chroma until the colour is displayable,
  descending all the way to neutral so it cannot fall through to a clipped
  result.

## Exposure

Three surfaces, because each reaches a different audience:

| Surface | Who it is for |
|---|---|
| JSON file at `[theme] output_path`, written atomically | anything that can read a file |
| `ThemeChanged` IPC event | live subscribers; a bar reads this, no file or watcher needed |
| Spawn environment (`RUBIX_THEME`, `RUBIX_BACKGROUND`, …) | processes started *after* the change only |

`[theme] on_change` runs detached after each write. Extraction rides the
slideshow's existing off-thread decode; `resolve` and `set` extract
synchronously, on calls that already block for the decode itself.

## Config

See `config/default/` for the annotated `[theme]` block. `enable` and
`apply_to_borders` both default to **false** — silently overriding an
`active_color` the user typed is worse than doing nothing.

## Rendering into applications

`~/Scripts/rubixTheme.py` (in `dotfiles-tools`) is what `on_change` runs. Each
application needed a different mechanism; see that script's docstrings. The
tmux case is the instructive one: rather than pushing colours into tmux, its
pane styles are set to `default` so the terminal's own themed background shows
through. There is then no second copy of the background to drift.
