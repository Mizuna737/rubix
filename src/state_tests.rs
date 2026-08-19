use std::time::Instant;

use crate::state::{Pos, RubixState};
use crate::model::geometry::Rect;
use std::collections::HashMap;

fn make_rect(x: u32, y: u32, w: u32, h: u32) -> Rect {
    Rect { x, y, width: w, height: h }
}

fn pos(x: i32, y: i32) -> Pos {
    Pos { x, y }
}

// ---- ease ----

#[test]
fn ease_zero_is_zero() {
    assert_eq!(RubixState::ease(0.0), 0.0);
}

#[test]
fn ease_one_is_one() {
    assert_eq!(RubixState::ease(1.0), 1.0);
}

#[test]
fn ease_clamps_below_zero() {
    assert_eq!(RubixState::ease(-0.5), RubixState::ease(0.0));
}

#[test]
fn ease_clamps_above_one() {
    assert_eq!(RubixState::ease(1.5), RubixState::ease(1.0));
}

#[test]
fn ease_is_monotonic() {
    let step = 0.01;
    let mut prev = RubixState::ease(0.0);
    for i in 1..=100u32 {
        let t = i as f32 * step;
        let val = RubixState::ease(t);
        assert!(val >= prev, "ease not monotonic at t={}", t);
        prev = val;
    }
}

#[test]
fn ease_out_is_above_linear() {
    for i in 1..100u32 {
        let t = i as f32 / 100.0;
        let val = RubixState::ease(t);
        assert!(val > t, "ease({}) should be > {} for ease-out", t, t);
    }
}

// ---- lerp_pos ----

#[test]
fn lerp_pos_at_zero_equals_from() {
    let from = pos(10, 20);
    let to = pos(30, 40);
    let result = RubixState::lerp_pos(from, to, 0.0);
    assert_eq!(result, from);
}

#[test]
fn lerp_pos_at_one_equals_to() {
    let from = pos(10, 20);
    let to = pos(30, 40);
    let result = RubixState::lerp_pos(from, to, 1.0);
    assert_eq!(result, to);
}

#[test]
fn lerp_pos_midpoint_is_rounded_average() {
    let from = pos(0, 0);
    let to = pos(10, 10);
    let result = RubixState::lerp_pos(from, to, 0.5);
    assert_eq!(result.x, 5);
    assert_eq!(result.y, 5);
}

#[test]
fn lerp_pos_midpoint_of_even_odd() {
    let from = pos(0, 0);
    let to = pos(9, 9);
    let result = RubixState::lerp_pos(from, to, 0.5);
    // (0 + 9*0.5) = 4.5 -> round to 4 or 5
    assert!(result.x == 4 || result.x == 5);
    assert!(result.y == 4 || result.y == 5);
}

#[test]
fn lerp_pos_handles_negative_from() {
    // A leave-to-top / enter-from-above endpoint can legitimately be negative
    // now that coordinates are signed (no clamp).
    let from = pos(0, -500);
    let to = pos(0, 100);
    let result = RubixState::lerp_pos(from, to, 0.0);
    assert_eq!(result.y, -500);
}

// ---- plan_transition: Scroll ----

fn plan_scroll(
    current: HashMap<u32, Pos>,
    targets: &[(u32, Rect)],
    down: bool,
) -> HashMap<u32, crate::state::Tween> {
    let now = Instant::now();
    // Use a generous bounds height for testing
    let bounds = make_rect(0, 0, 1920, 1080);
    let transition = crate::state::Transition::Scroll { down };
    RubixState::plan_transition(&current, targets, transition, bounds, now)
}

#[test]
fn plan_scroll_down_move_in_both() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    current.insert(1, pos(0, 100));
    let targets = vec![(1, make_rect(0, 150, 200, 300))];
    let tweens = plan_scroll(current, &targets, true);
    assert!(tweens.contains_key(&1));
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Move);
    assert_eq!(tween.from, pos(0, 100));
    assert_eq!(tween.to, pos(0, 150));
}

#[test]
fn plan_scroll_down_enter_in_targets_only() {
    let current: HashMap<u32, Pos> = HashMap::new();
    let target = make_rect(0, 0, 200, 300);
    let targets = vec![(1, target)];
    let tweens = plan_scroll(current, &targets, true);
    assert!(tweens.contains_key(&1));
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Enter);
    assert_eq!(tween.to, pos(target.x as i32, target.y as i32));
    assert_eq!(tween.from.x, target.x as i32);
    // enter from BELOW: from.y = target.y + bounds.height
    assert_eq!(tween.from.y, target.y as i32 + 1080);
}

#[test]
fn plan_scroll_down_leave_in_current_only() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let cur = pos(0, 100);
    current.insert(1, cur);
    let targets: Vec<(u32, Rect)> = vec![];
    let tweens = plan_scroll(current, &targets, true);
    assert!(tweens.contains_key(&1));
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Leave);
    assert_eq!(tween.from, cur);
    // leave to TOP: to.y = current.y - bounds.height -- NO clamp, may go negative.
    assert_eq!(tween.to.y, 100 - 1080);
    assert_eq!(tween.to.x, cur.x);
}

#[test]
fn plan_scroll_down_enter_from_below() {
    let current: HashMap<u32, Pos> = HashMap::new();
    let target = make_rect(0, 500, 200, 300);
    let targets = vec![(1, target)];
    let tweens = plan_scroll(current, &targets, true);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Enter);
    assert_eq!(tween.from.x, target.x as i32);
    assert_eq!(tween.from.y, target.y as i32 + 1080);
}

#[test]
fn plan_scroll_down_leave_to_top_goes_negative() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let cur = pos(0, 400);
    current.insert(1, cur);
    let targets: Vec<(u32, Rect)> = vec![];
    let tweens = plan_scroll(current, &targets, true);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Leave);
    // to.y = current.y - bounds.height, unclamped -- this is the bug fix:
    // a near-top group must slide fully off-screen (negative), not freeze at 0.
    assert_eq!(tween.to.y, 400 - 1080);
    assert!(tween.to.y < 0);
}

#[test]
fn plan_scroll_up_enter_from_above_is_negative() {
    let current: HashMap<u32, Pos> = HashMap::new();
    let target = make_rect(0, 500, 200, 300);
    let targets = vec![(1, target)];
    let tweens = plan_scroll(current, &targets, false);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Enter);
    assert_eq!(tween.from.x, target.x as i32);
    // enter from ABOVE: from.y = target.y - bounds.height, unclamped.
    assert_eq!(tween.from.y, 500 - 1080);
}

#[test]
fn plan_scroll_up_leave_to_bottom() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let cur = pos(0, 200);
    current.insert(1, cur);
    let targets: Vec<(u32, Rect)> = vec![];
    let tweens = plan_scroll(current, &targets, false);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Leave);
    // to.y = current.y + bounds.height
    assert_eq!(tween.to.y, 200 + 1080);
}

#[test]
fn plan_empty_current_one_target_gives_enter() {
    let current: HashMap<u32, Pos> = HashMap::new();
    let target = make_rect(0, 0, 200, 300);
    let targets = vec![(1, target)];
    let tweens = plan_scroll(current, &targets, true);
    assert_eq!(tweens.len(), 1);
    assert!(tweens.contains_key(&1));
    assert_eq!(tweens[&1].kind, crate::state::TweenKind::Enter);
    assert_eq!(tweens[&1].to, pos(target.x as i32, target.y as i32));
}

#[test]
fn plan_both_current_and_targets_populated() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    current.insert(1, pos(0, 0));
    current.insert(2, pos(100, 0));
    // only window 1 is still visible; window 2 is leaving
    let targets = vec![(1, make_rect(0, 50, 100, 200))];
    let tweens = plan_scroll(current, &targets, true);
    assert_eq!(tweens.len(), 2);
    assert!(tweens.contains_key(&1));
    assert_eq!(tweens[&1].kind, crate::state::TweenKind::Move);
    assert!(tweens.contains_key(&2));
    assert_eq!(tweens[&2].kind, crate::state::TweenKind::Leave);
}

// ---- plan_transition: Rotate ----

fn plan_rotate(
    current: HashMap<u32, Pos>,
    targets: &[(u32, Rect)],
) -> HashMap<u32, crate::state::Tween> {
    let now = Instant::now();
    let bounds = make_rect(0, 0, 1920, 1080);
    RubixState::plan_transition(&current, targets, crate::state::Transition::Rotate, bounds, now)
}

#[test]
fn plan_rotate_enter_right_half_slides_in_from_right() {
    let current: HashMap<u32, Pos> = HashMap::new();
    // target x=1500 is in the right half of a 1920-wide bounds (midpoint 960)
    let target = make_rect(1500, 0, 200, 300);
    let targets = vec![(1, target)];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Enter);
    assert_eq!(tween.from.x, target.x as i32 + 1920);
    assert_eq!(tween.from.y, target.y as i32);
    assert_eq!(tween.to, pos(target.x as i32, target.y as i32));
}

#[test]
fn plan_rotate_enter_left_half_slides_in_from_left() {
    let current: HashMap<u32, Pos> = HashMap::new();
    // target x=100 is in the left half of a 1920-wide bounds (midpoint 960)
    let target = make_rect(100, 0, 200, 300);
    let targets = vec![(1, target)];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Enter);
    assert_eq!(tween.from.x, target.x as i32 - 1920);
    assert_eq!(tween.from.y, target.y as i32);
}

#[test]
fn plan_rotate_leave_from_right_half_exits_right() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let cur = pos(1500, 0);
    current.insert(1, cur);
    let targets: Vec<(u32, Rect)> = vec![];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Leave);
    assert_eq!(tween.from, cur);
    assert_eq!(tween.to.x, cur.x + 1920);
    assert_eq!(tween.to.y, cur.y);
}

#[test]
fn plan_rotate_leave_from_left_half_exits_left() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let cur = pos(100, 0);
    current.insert(1, cur);
    let targets: Vec<(u32, Rect)> = vec![];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Leave);
    assert_eq!(tween.to.x, cur.x - 1920);
}

#[test]
fn plan_rotate_move_goes_current_to_target_unchanged() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    current.insert(1, pos(0, 0));
    let target = make_rect(500, 0, 200, 300);
    let targets = vec![(1, target)];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Move);
    assert_eq!(tween.from, pos(0, 0));
    assert_eq!(tween.to, pos(target.x as i32, target.y as i32));
}

// ---- plan_transition: Rotate wrap (ghost) ----
//
// Render injection (the actual second draw call in winit.rs/udev.rs) is not
// unit-testable here -- it needs a live renderer/backend. These tests only
// cover the pure wrap-core math in `plan_rotate_move`.

#[test]
fn plan_rotate_wrap_right_to_left_sets_ghost() {
    // 3 columns, bounds width 1920 -> cw = 640. Window moves from the
    // rightmost column (x=1280) to the leftmost (x=0): long_delta = -1280,
    // |long_delta| = 1280 > 960 (width/2) -> wrap.
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let orig_from = pos(1280, 0);
    current.insert(1, orig_from);
    let orig_to = make_rect(0, 0, 640, 1080);
    let targets = vec![(1, orig_to)];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];

    assert_eq!(tween.kind, crate::state::TweenKind::Move);
    // wrap_delta = long_delta + width = -1280 + 1920 = 640
    let wrap_delta = 640;
    // Space copy: lands at the real destination.
    assert_eq!(tween.to, pos(0, 0));
    assert_eq!(tween.from, pos(0 - wrap_delta, 0));
    // Ghost: exits off the near edge, starting where the window is now.
    let ghost = tween.ghost.expect("wrap should set a ghost");
    assert_eq!(ghost.from, orig_from);
    assert_eq!(ghost.to, pos(orig_from.x + wrap_delta, orig_from.y));
}

#[test]
fn plan_rotate_wrap_left_to_right_sets_ghost() {
    // Mirror direction: window moves from leftmost (x=0) to rightmost (x=1280).
    // long_delta = 1280 > 960 -> wrap; wrap_delta = 1280 - 1920 = -640.
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let orig_from = pos(0, 0);
    current.insert(1, orig_from);
    let orig_to = make_rect(1280, 0, 640, 1080);
    let targets = vec![(1, orig_to)];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];

    let wrap_delta = -640;
    assert_eq!(tween.to, pos(1280, 0));
    assert_eq!(tween.from, pos(1280 - wrap_delta, 0));
    let ghost = tween.ghost.expect("wrap should set a ghost");
    assert_eq!(ghost.from, orig_from);
    assert_eq!(ghost.to, pos(orig_from.x + wrap_delta, orig_from.y));
}

#[test]
fn plan_rotate_threshold_boundary_is_not_a_wrap() {
    // Exactly width/2 (a 2-column full swap on a 1920-wide bounds: delta 960)
    // must NOT be treated as a wrap -- strict `>` only.
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let orig_from = pos(0, 0);
    current.insert(1, orig_from);
    let orig_to = make_rect(960, 0, 960, 1080);
    let targets = vec![(1, orig_to)];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];

    assert_eq!(tween.ghost, None);
    assert_eq!(tween.kind, crate::state::TweenKind::Move);
    assert_eq!(tween.from, orig_from);
    assert_eq!(tween.to, pos(960, 0));
}

#[test]
fn plan_rotate_short_adjacent_move_is_not_a_wrap() {
    // |delta| < width/2: a short, plain Move -- no ghost.
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let orig_from = pos(0, 0);
    current.insert(1, orig_from);
    let orig_to = make_rect(500, 0, 200, 300);
    let targets = vec![(1, orig_to)];
    let tweens = plan_rotate(current, &targets);
    let tween = tweens[&1];

    assert_eq!(tween.ghost, None);
    assert_eq!(tween.from, orig_from);
    assert_eq!(tween.to, pos(500, 0));
}

// ---- reveal: shrink/grow in place ----
//
// The reveal swap trades two NON-adjacent groups: the displaced one is headed
// for a column that is off-screen by definition, so there is no edge to travel
// toward. Both endpoints of every reveal tween are therefore the same point,
// and all the motion lives in the scale channel.

fn plan_reveal(
    current: HashMap<u32, Pos>,
    targets: &[(u32, Rect)],
) -> HashMap<u32, crate::state::Tween> {
    let bounds = make_rect(0, 0, 1920, 1080);
    RubixState::plan_transition(
        &current,
        targets,
        crate::state::Transition::Reveal,
        bounds,
        Instant::now(),
    )
}

#[test]
fn plan_reveal_enter_grows_from_nothing_in_place() {
    let current: HashMap<u32, Pos> = HashMap::new();
    let target = make_rect(300, 200, 400, 500);
    let tweens = plan_reveal(current, &vec![(1, target)]);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Enter);
    // No slide: unlike Scroll/Rotate, `from` is not offset off-screen.
    assert_eq!(tween.from, pos(300, 200));
    assert_eq!(tween.to, pos(300, 200));
    assert_eq!(tween.scale, Some(crate::state::ScaleTrack { from: 0.0, to: 1.0 }));
}

#[test]
fn plan_reveal_leave_shrinks_to_nothing_in_place() {
    let mut current: HashMap<u32, Pos> = HashMap::new();
    let cur = pos(300, 200);
    current.insert(1, cur);
    let tweens = plan_reveal(current, &vec![]);
    let tween = tweens[&1];
    assert_eq!(tween.kind, crate::state::TweenKind::Leave);
    assert_eq!(tween.from, cur);
    assert_eq!(tween.to, cur, "a shrinking window must not also travel off-screen");
    assert_eq!(tween.scale, Some(crate::state::ScaleTrack { from: 1.0, to: 0.0 }));
}

#[test]
fn plan_reveal_leaves_untouched_columns_unscaled() {
    // Only two groups swap; every other visible column holds still. Those
    // windows must stay on the cheap Space-mapped path -- a scale channel here
    // would unmap them and hand them to the hand-rolled render list for no
    // reason.
    let mut current: HashMap<u32, Pos> = HashMap::new();
    current.insert(7, pos(900, 0));
    let tweens = plan_reveal(current, &vec![(7, make_rect(900, 0, 300, 600))]);
    let tween = tweens[&7];
    assert_eq!(tween.kind, crate::state::TweenKind::Move);
    assert_eq!(tween.scale, None);
    assert_eq!(tween.from, tween.to);
}

#[test]
fn plan_reveal_swaps_both_directions_at_once() {
    // The real shape of a swap: the revealed group enters while the displaced
    // group leaves, in the same plan, at the same slot.
    let mut current: HashMap<u32, Pos> = HashMap::new();
    current.insert(1, pos(0, 0));                       // displaced
    let targets = vec![(10, make_rect(0, 0, 300, 600))]; // revealed
    let tweens = plan_reveal(current, &targets);
    assert_eq!(tweens[&1].kind, crate::state::TweenKind::Leave);
    assert_eq!(tweens[&1].scale.unwrap().to, 0.0);
    assert_eq!(tweens[&10].kind, crate::state::TweenKind::Enter);
    assert_eq!(tweens[&10].scale.unwrap().from, 0.0);
    // Both animate over the same rect, which is what reads as a trade.
    assert_eq!(tweens[&1].from, tweens[&10].to);
}

// ---- lerp_scale ----

#[test]
fn lerp_scale_hits_both_endpoints() {
    let track = crate::state::ScaleTrack { from: 0.0, to: 1.0 };
    assert_eq!(RubixState::lerp_scale(track, 0.0), 0.0);
    assert_eq!(RubixState::lerp_scale(track, 1.0), 1.0);
}

#[test]
fn lerp_scale_runs_backwards_for_a_shrink() {
    let track = crate::state::ScaleTrack { from: 1.0, to: 0.0 };
    assert_eq!(RubixState::lerp_scale(track, 0.0), 1.0);
    assert_eq!(RubixState::lerp_scale(track, 0.5), 0.5);
    assert_eq!(RubixState::lerp_scale(track, 1.0), 0.0);
}

// ---- group_bounds (the Group maximize stage's rect) ----

use crate::state::group_bounds;

#[test]
fn group_bounds_of_no_matching_tiles_is_none() {
    // The off-screen case: a scrolled-away group contributes no entries to
    // `targets`, and must not be blown up.
    let targets = vec![(1, make_rect(0, 0, 100, 100))];
    assert_eq!(group_bounds(&targets, &[7, 8]), None);
}

#[test]
fn group_bounds_of_empty_id_list_is_none() {
    let targets = vec![(1, make_rect(0, 0, 100, 100))];
    assert_eq!(group_bounds(&targets, &[]), None);
}

#[test]
fn group_bounds_of_a_lone_tile_is_that_tile() {
    let tile = make_rect(30, 40, 200, 300);
    let targets = vec![(1, tile)];
    assert_eq!(group_bounds(&targets, &[1]), Some(tile));
}

#[test]
fn group_bounds_unions_across_a_split_seam() {
    // Two tiles stacked with a 10px seam between them: the union must span the
    // seam, not report either half.
    let targets = vec![
        (1, make_rect(100, 0, 400, 295)),
        (2, make_rect(100, 305, 400, 295)),
    ];
    assert_eq!(group_bounds(&targets, &[1, 2]), Some(make_rect(100, 0, 400, 600)));
}

#[test]
fn group_bounds_ignores_tiles_outside_the_group() {
    // Neighbouring columns are in `targets` too; including them would blow the
    // window up across the whole screen instead of its own band.
    let targets = vec![
        (1, make_rect(0, 0, 300, 600)),
        (2, make_rect(310, 0, 300, 600)),
        (3, make_rect(620, 0, 300, 600)),
    ];
    assert_eq!(group_bounds(&targets, &[2]), Some(make_rect(310, 0, 300, 600)));
}

// The claim the whole approach rests on: the union of a group's tiles IS the
// column band `compute_layout` laid them into. If this ever fails, the Group
// stage is misaligned against the grid and the rect must be computed from the
// band arithmetic directly instead of derived from the tiles.
#[test]
fn group_bounds_reproduces_the_column_band_exactly() {
    use crate::model::grid::{Column, Group, Monitor};
    use crate::model::tiling::{SplitDirection, TilingNode};

    let bounds = make_rect(0, 0, 1920, 1080);
    let (outer_gap, inner_gap) = (20, 10);

    // Same grid twice, differing only in how many windows sit in column 1's
    // group: one window, versus a three-way split.
    let build = |middle: Group| {
        let mut mon = Monitor::new(0, 0);
        for group in [Group::new(TilingNode::Leaf { window_id: 1 }, 1), middle, Group::new(TilingNode::Leaf { window_id: 9 }, 9)] {
            let mut col = Column::new(0);
            col.add_group(group);
            mon.add_column(col);
        }
        mon.increment_visible_columns(3);
        mon
    };

    let mut split = Group::new(TilingNode::Leaf { window_id: 2 }, 2);
    split.add_window(SplitDirection::Vertical, 3, 2);
    split.add_window(SplitDirection::Horizontal, 4, 3);

    let lone = build(Group::new(TilingNode::Leaf { window_id: 2 }, 2));
    let many = build(split);

    let band = lone
        .compute_layout(bounds, outer_gap, inner_gap)
        .into_iter()
        .find(|(id, _)| *id == 2)
        .map(|(_, rect)| rect)
        .expect("the lone window's tile is its whole band");

    let targets = many.compute_layout(bounds, outer_gap, inner_gap);
    assert_eq!(
        group_bounds(&targets, &[2, 3, 4]),
        Some(band),
        "a split group's tiles must union back to the band a lone window fills"
    );
}
