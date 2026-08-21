# HDR status

**Current as of 2026-08-20.** HDR is live and composited — not fullscreen-only. The frame is built
in a linear working space and encoded to PQ, with per-window decode for PQ and Windows-scRGB
content, and tone-mapping down for SDR outputs and for every capture path (screenshots, portal
screencast). HDR games work through GE-Proton + gamescope. Idle HDR compositing costs no more than
SDR after element-level damage tracking. The compositor draws its own wallpaper, so an HDR AVIF can
drive a connector into HDR mode with no HDR window present.

Everything below this line is the **2026-07-31 snapshot** that planned the work — kept for the
measurements, the hardware findings, and the prior-art list. Its "what to test" and "plan" sections
are historical; the live sprint doc is `Rubix-Roadmap` in the Obsidian vault.

---

## Snapshot — dual-head, rotation, HDR (2026-07-30 → 07-31)

Written by Claude while you slept. TL;DR: **multi-monitor + rotation are done and ready
to test; your dual-head config is written. HDR groundwork landed gated-off, but real HDR
is blocked upstream in Smithay 0.7 — details and a full plan below.** Nothing is enabled by
default, so none of this can disturb your session.

---

## 1. What to test first thing (your actionable item)

Your `~/.config/rubix/config.toml` now has an `[[output]]` block (stow → tracked in
`~/dotfiles`). Restart Rubix and check:

- **Both heads light** at their positions: ultrawide `DP-3` at (0,0), the strip `HDMI-A-1`
  **rotated to 1280×400 landscape, directly below** the primary at (1036, 1440).
- **Rotation direction.** The strip is a physically-portrait `400x1280` panel; the config
  uses `transform = "right"` (= xrandr `--rotate right`). The smithay CW/CCW convention is a
  best guess — **if the strip is upside-down / mirrored, change `transform = "right"` to
  `"left"`** in the config (hot-reloads; connect is re-read on restart to be safe).
- **Windows tile independently per head** — spawn something on the strip vs the ultrawide.
- **Cursor** crosses between heads and stops at the outer edges (no dead-zone escape).
- **Screen-share the strip in Teams** → should now show the correct content (this was the
  Stage-4 capture follow-up, resolved by output positioning).

Config written:
```toml
[[output]]
name = "DP-3"
position = [0, 0]
mode = "3440x1440"
primary = true

[[output]]
name = "HDMI-A-1"
mode = "400x1280"
transform = "right"     # ← flip to "left" if the strip renders mirrored
position = [1036, 1440]
```

---

## 2. What landed (commits on `main`, all reviewed + tests green)

| Commit | What |
|--------|------|
| `a4109fb` | Multi-monitor Stage 2 — `Workspace`/`Vec<Monitor>` wired end-to-end (from earlier) |
| `4d1064b` | Output `transform` (rotation) in `[[output]]` config |
| `409fe51` | HDR groundwork — config toggle + ST 2086 metadata blob, **gated off** |

`cargo test`: 129 passed, 0 failed. Default config + any non-HDR output take the identical
code path as before.

---

## 3. HDR — honest status

**Feasibility on your hardware is confirmed.** A read-only DRM probe shows `DP-3` exposes
both `HDR_OUTPUT_METADATA` (property id 8) and `Colorspace` (ids 824/827/831/835), on the
open NVIDIA kernel module 610.43.03 / kernel 7.1.5. 10-bit scanout (`XRGB2101010`) is
already first in the DRM format list. So the display *can* do HDR.

**What I built (gated behind per-output `hdr = true`, default false):**
- `src/hdr.rs` — `HdrMetadata` + `build_hdr_metadata_blob()` packing the exact 32-byte Linux
  `hdr_output_metadata` uAPI (PQ EOTF, static-metadata type 1, BT.2020 primaries, D65 white
  point, mastering luminance, max_cll/fall). **12 unit tests pin every byte offset** — this
  part is correct and reusable.
- `src/udev.rs` — `set_hdr_output_properties()` (only runs when `hdr = true`) verifies the
  connector exposes the properties and builds the blob.

**Why it's a no-op today — two upstream blockers in Smithay 0.7:**

1. **Can't apply the metadata.** Smithay's `DrmCompositor`/`AtomicDrmSurface` own the atomic
   commit and expose **no public hook** to fold an arbitrary connector property
   (`HDR_OUTPUT_METADATA`, `Colorspace`) into that commit. Setting it out-of-band via drm-rs
   `set_property` would issue a legacy set that **races with / gets clobbered by** Smithay's
   atomic commits on atomic-only drivers. So the blob is built but deliberately **not
   applied** — the alternative was a fragile hack, which I would not commit.

2. **No color/tone-map render pipeline.** Even with metadata applied, the compositor renders
   SDR content in an sRGB-ish 8-bit pipeline. Pushing that to a 10-bit PQ/BT.2020 output
   *without tone mapping* looks **worse** than SDR (washed-out, grey). Real HDR needs
   linear-light compositing + SDR→HDR tone mapping in the GLES renderer. Smithay provides
   **none** of this; per the maintainer, "a lot of changes to our rendering pipeline, which
   hasn't been started at all" (smithay#1143).

**This is not a Rubix gap — it's an ecosystem gap.** cosmic-comp (#1384) and niri (#1128),
both Smithay-based, have **not** shipped HDR; they're waiting on the same upstream work. Only
KWin and Gamescope (both non-Smithay) have it, plus an experimental niri community fork
(`dividebysandwich`) riding the draft smithay#1143 branch.

---

## 4. Plan to actually get HDR working (in dependency order)

**Blocker A — apply connector properties.** Options, best first:
  - (a) **Vendor/patch Smithay** to add a public hook that folds extra
    `(connector, prop, value)` pairs into every atomic commit/test alongside its managed set
    — mirroring how it already handles `VRR_ENABLED` internally. Smallest real unblock;
    upstreamable. Requires a `[patch.crates-io]` smithay fork in Cargo.toml (deliberately not
    done overnight — vendoring a core dep unattended is too destabilizing).
  - (b) Track/adopt **smithay#1143** (draft color-management branch) once it stabilizes — it
    reworks the pipeline and is the eventual "real" path; the niri fork already rides it.
  - (c) A device-level property set carefully synchronized to Smithay's commit cadence — a
    hack; not recommended.

**Blocker B — render pipeline (the big one).**
  1. 10-bit *linear* offscreen compositing (currently sRGB 8-bit).
  2. Per-content EOTF handling; SDR→HDR **inverse tone mapping** (ITM) in a frag shader.
  3. Output EOTF (PQ / ST 2084) encode on the final pass.
  4. Peak luminance from the display EDID (`max_cll` etc.), à la KWin.

**Then — client signalling.** Hand-roll `wp_color_management_v1` (stable since
wayland-protocols 1.41, Feb 2025; **not** in Smithay 0.7 — custom `wl_global`, deserialize
the XML) so HDR-native clients (video, games) advertise their color space and get
pass-through instead of tone-mapping.

**Reference — the tone-map math** (ready to drop into the eventual frag shader; PQ / ST 2084
encode + Gamescope-style ITM):
```glsl
// SDR (2.2 gamma) sample -> linear
vec3 lin = pow(sdr, vec3(2.2));
// inverse tone map: SDR ~100 nits -> HDR target (e.g. 1000 nits), in linear light
lin *= (target_nits / 100.0);
// normalize to 10000-nit PQ domain, then ST 2084 encode
vec3 y = lin * (target_nits / 10000.0);
const float m1 = 0.1593017578125, m2 = 78.84375;
const float c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
vec3 yp = pow(y, vec3(m1));
vec3 pq = pow((c1 + c2*yp) / (1.0 + c3*yp), vec3(m2));  // -> feed 10-bit PQ output
```

**Recommended stance:** the highest-leverage single step is **Blocker A option (a)** — a small
Smithay patch to allow setting the connector properties. That alone lets you flip `DP-3` into
HDR *signalling* mode and confirm the metadata path end-to-end on your panel. The render
pipeline (Blocker B) is genuinely multi-session work and is the real cost of "HDR" — worth
tracking smithay#1143 rather than hand-rolling the whole color pipeline from scratch.

---

## 5. Prior-art references
- Smithay color-management PR/status: github.com/Smithay/smithay issues/1143
- cosmic-comp HDR: github.com/pop-os/cosmic-comp issues/1384
- niri HDR discussion: github.com/niri-wm/niri discussions/1128 (+ `dividebysandwich` fork)
- KWin HDR write-up: zamundaaa.github.io/wayland/2024/05/11/more-hdr-and-color.html
- Gamescope ITM/tone-map: github.com/ValveSoftware/gamescope pull/714
- Kernel uAPI: include/uapi/drm/drm_mode.h (`hdr_output_metadata`)
- wp_color_management_v1: wayland.app/protocols/color-management-v1
