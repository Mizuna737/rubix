    use super::*;
    use super::super::grid::{Column, Group};
    use super::super::tiling::{SplitDirection, TilingNode};

    // ---- builders ----
    const BOUNDS: Rect = Rect { x: 0, y: 0, width: 1920, height: 1080 };

    fn leaf_group(id: u32) -> Group {
        Group::new(TilingNode::Leaf { window_id: id }, id)
    }
    fn hsplit_group(left: u32, right: u32) -> Group {
        Group::new(TilingNode::Split {
            split_direction: SplitDirection::Horizontal,
            split_ratio: 0.5,
            left_child: Box::new(TilingNode::Leaf { window_id: left }),
            right_child: Box::new(TilingNode::Leaf { window_id: right }),
        }, left)
    }
    fn column_of(groups: Vec<Group>) -> Column {
        let mut column = Column::new(0);
        for group in groups {
            column.add_group(group);
        }
        column
    }
    /// Build a monitor with exactly `columns`, then open `visible` of them.
    /// Monitor::new(id, 0) seeds nothing, so the caller owns the whole shape --
    /// increment_visible_columns clamps into [1, columns.len()].
    fn monitor_of(columns: Vec<Column>, visible: usize) -> Monitor {
        let mut monitor = Monitor::new(0, 0);
        for column in columns {
            monitor.add_column(column);
        }
        monitor.increment_visible_columns(visible as isize);
        monitor
    }
    fn slot_at(slots: &[CubeSlot], column: usize, row: usize) -> &CubeSlot {
        slots.iter()
            .find(|s| s.column == column && s.row == row)
            .unwrap_or_else(|| panic!("no slot at ({column}, {row})"))
    }

    // ---- from_rect: the coordinate convention ----

    #[test]
    fn from_rect_leaves_everything_but_y_alone() {
        let rect = Rect { x: 100, y: 200, width: 300, height: 400 };
        for offset in [-2, -1, 0, 1, 2] {
            let cube = CubeRect::from_rect(rect, offset, 1080);
            assert_eq!(cube.x, 100);
            assert_eq!(cube.width, 300);
            assert_eq!(cube.height, 400);
        }
    }

    #[test]
    fn from_rect_row_offset_zero_is_identity_on_y() {
        let rect = Rect { x: 0, y: 200, width: 100, height: 100 };
        assert_eq!(CubeRect::from_rect(rect, 0, 1080).y, 200);
    }

    #[test]
    fn from_rect_shifts_by_exactly_one_band_per_row() {
        // Negative offsets are rows scrolled off the TOP, so they go negative in
        // screen coords (y grows down). This is what makes Direction::Up == -y.
        let rect = Rect { x: 0, y: 0, width: 100, height: 1080 };
        assert_eq!(CubeRect::from_rect(rect, -1, 1080).y, -1080);
        assert_eq!(CubeRect::from_rect(rect, 1, 1080).y, 1080);
        assert_eq!(CubeRect::from_rect(rect, 3, 1080).y, 3240);
    }

    #[test]
    fn from_rect_adjacent_rows_tile_exactly() {
        let rect = Rect { x: 0, y: 0, width: 100, height: 1080 };
        let above = CubeRect::from_rect(rect, -1, 1080);
        let here = CubeRect::from_rect(rect, 0, 1080);
        let below = CubeRect::from_rect(rect, 1, 1080);
        assert_eq!(above.bottom(), here.top());
        assert_eq!(here.bottom(), below.top());
    }

    // ---- column_band: the x plane ----

    #[test]
    fn column_bands_tile_exactly_and_cover_the_full_width() {
        // 1920 / 7 does not divide evenly; the multiply-then-divide form must
        // absorb the remainder rather than leaving pixels unclaimed on the right.
        let monitor = monitor_of((0..7).map(|_| column_of(vec![Group::empty()])).collect(), 7);
        let mut previous_right = BOUNDS.x;
        for c in 0..7 {
            let (left, right) = column_band(&monitor, BOUNDS, c);
            assert_eq!(left, previous_right, "band {c} does not abut its predecessor");
            assert!(right > left, "band {c} is empty");
            previous_right = right;
        }
        assert_eq!(previous_right, BOUNDS.x + BOUNDS.width);
    }

    #[test]
    fn column_bands_honour_the_output_origin() {
        let bounds = Rect { x: 1920, y: 0, width: 1920, height: 1080 };
        let monitor = monitor_of((0..2).map(|_| column_of(vec![Group::empty()])).collect(), 2);
        assert_eq!(column_band(&monitor, bounds, 0).0, 1920);
        assert_eq!(column_band(&monitor, bounds, 1).1, 3840);
    }

    #[test]
    fn column_bands_extend_past_the_visible_range() {
        // Four columns, two visible: the cube keeps going to the right at the
        // current zoom rather than compressing to fit.
        let monitor = monitor_of((0..4).map(|_| column_of(vec![Group::empty()])).collect(), 2);
        assert_eq!(column_band(&monitor, BOUNDS, 2).0, 1920);
        assert_eq!(column_band(&monitor, BOUNDS, 3).0, 2880);
    }

    // ---- compute_cube ----

    #[test]
    fn single_window_fills_the_whole_plane() {
        let monitor = monitor_of(vec![column_of(vec![leaf_group(1)])], 1);
        let slots = compute_cube(&monitor, BOUNDS);
        assert_eq!(slots.len(), 1);
        let slot = &slots[0];
        assert_eq!(slot.rect, CubeRect { x: 0, y: 0, width: 1920, height: 1080 });
        assert_eq!(slot.windows, vec![(1, slot.rect)]);
    }

    #[test]
    fn rows_stack_relative_to_their_own_active_row() {
        let mut column = column_of(vec![leaf_group(1), leaf_group(2), leaf_group(3)]);
        column.scroll_column(1); // active_row = 1
        let slots = compute_cube(&monitor_of(vec![column], 1), BOUNDS);

        // The active row sits where it renders; the others are one band away each.
        assert_eq!(slot_at(&slots, 0, 1).rect.top(), 0);
        assert_eq!(slot_at(&slots, 0, 0).rect.top(), -1080);
        assert_eq!(slot_at(&slots, 0, 2).rect.top(), 1080);
    }

    #[test]
    fn every_column_stacks_around_its_own_active_row() {
        // The cube is N independent carousels, not a grid: column 1 scrolled to
        // row 2 must still place ITS active row at y == 0, level with column 0's.
        let mut scrolled = column_of(vec![leaf_group(10), leaf_group(11), leaf_group(12)]);
        scrolled.scroll_column(2);
        let monitor = monitor_of(vec![column_of(vec![leaf_group(1)]), scrolled], 2);
        let slots = compute_cube(&monitor, BOUNDS);

        assert_eq!(slot_at(&slots, 0, 0).rect.top(), 0);
        assert_eq!(slot_at(&slots, 1, 2).rect.top(), 0);
        assert_eq!(slot_at(&slots, 1, 0).rect.top(), -2160);
    }

    #[test]
    fn adjacent_columns_share_an_edge_on_the_same_row() {
        let monitor = monitor_of(
            (0..3).map(|i| column_of(vec![leaf_group(i + 1)])).collect(),
            3,
        );
        let slots = compute_cube(&monitor, BOUNDS);
        assert_eq!(slot_at(&slots, 0, 0).rect.right(), slot_at(&slots, 1, 0).rect.left());
        assert_eq!(slot_at(&slots, 1, 0).rect.right(), slot_at(&slots, 2, 0).rect.left());
    }

    #[test]
    fn empty_groups_still_get_a_slot() {
        // An empty group is a legitimate destination -- focus lands there so a
        // spawn has somewhere to go -- so it needs geometry even with no windows.
        let monitor = monitor_of(vec![column_of(vec![leaf_group(1), Group::empty()])], 1);
        let slots = compute_cube(&monitor, BOUNDS);
        assert_eq!(slots.len(), 2);
        let empty = slot_at(&slots, 0, 1);
        assert!(empty.windows.is_empty());
        assert_eq!(empty.rect.height, 1080);
    }

    #[test]
    fn columns_with_no_groups_emit_no_slots() {
        // Reachable state (reveal_window guards against it), but it is a model
        // invariant violation -- cube.rs must not paper over it with a phantom slot.
        let monitor = monitor_of(vec![column_of(vec![leaf_group(1)]), column_of(vec![])], 2);
        let slots = compute_cube(&monitor, BOUNDS);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].column, 0);
    }

    #[test]
    fn windows_are_contained_in_their_slot() {
        let mut scrolled = column_of(vec![hsplit_group(1, 2), hsplit_group(3, 4)]);
        scrolled.scroll_column(1);
        let monitor = monitor_of(vec![scrolled, column_of(vec![hsplit_group(5, 6)])], 2);
        for slot in compute_cube(&monitor, BOUNDS) {
            for (id, rect) in &slot.windows {
                assert!(
                    rect.left() >= slot.rect.left() && rect.right() <= slot.rect.right()
                        && rect.top() >= slot.rect.top() && rect.bottom() <= slot.rect.bottom(),
                    "window {id} at {rect:?} escapes slot {:?}", slot.rect,
                );
            }
        }
    }

    #[test]
    fn split_windows_tile_their_slot_gaplessly() {
        // inner_gap is deliberately 0 in cube space: leaves must share edges
        // exactly, because the nav candidate filter compares left() to right().
        let monitor = monitor_of(vec![column_of(vec![hsplit_group(1, 2)])], 1);
        let slots = compute_cube(&monitor, BOUNDS);
        let windows = &slots[0].windows;
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].1.right(), windows[1].1.left());
        assert_eq!(windows[0].1.left(), slots[0].rect.left());
        assert_eq!(windows[1].1.right(), slots[0].rect.right());
    }

    #[test]
    fn off_screen_columns_are_still_placed_on_the_plane() {
        let monitor = monitor_of(
            (0..4).map(|i| column_of(vec![leaf_group(i + 1)])).collect(),
            2,
        );
        let slots = compute_cube(&monitor, BOUNDS);
        assert_eq!(slots.len(), 4);
        assert_eq!(slot_at(&slots, 3, 0).rect.left(), 2880);
        assert_eq!(slot_at(&slots, 3, 0).rect.width, 960);
    }

    // ---- project: the sign convention ----

    fn cr(x: i32, y: i32, width: u32, height: u32) -> CubeRect {
        CubeRect { x, y, width, height }
    }

    #[test]
    fn right_travels_toward_increasing_x() {
        // Pinned against physical position, not against Left. A fully inverted
        // mapping is self-consistent and sails through any symmetry-only test.
        let left = cr(0, 0, 100, 100);
        let right = cr(100, 0, 100, 100);
        assert!(project(right, Direction::Right).near >= project(left, Direction::Right).far);
        assert!(project(right, Direction::Left).far <= project(left, Direction::Left).near);
    }

    #[test]
    fn down_travels_toward_increasing_y() {
        // y grows downward on screen, so Down is the un-negated direction.
        let upper = cr(0, 0, 100, 100);
        let lower = cr(0, 100, 100, 100);
        assert!(project(lower, Direction::Down).near >= project(upper, Direction::Down).far);
        assert!(project(lower, Direction::Up).far <= project(upper, Direction::Up).near);
    }

    #[test]
    fn project_puts_the_cross_axis_in_lo_hi() {
        let rect = cr(10, 20, 30, 40);
        for direction in [Direction::Left, Direction::Right] {
            let p = project(rect, direction);
            assert_eq!((p.lo, p.hi), (20, 60), "horizontal travel spans y across the beam");
        }
        for direction in [Direction::Up, Direction::Down] {
            let p = project(rect, direction);
            assert_eq!((p.lo, p.hi), (10, 40), "vertical travel spans x across the beam");
        }
    }

    // ---- nearest_in_direction ----
    //
    // The plane from the design walkthrough: 1920x1080, visible_columns 2.
    //
    //  x:  0        480       960                1920               2880
    //      +---------+---------+------------------+------------------+
    // y=0  |  win 1  |  win 2  |      win 3       |      win 5       |
    //      |  c0 r0  |  c0 r0  |      c1 r0       |      c2 r0       |
    // 1080 +---------+---------+------------------+------------------+
    //      |                   |      win 4       |
    //      |                   |      c1 r1       |
    // 2160 +-------------------+------------------+

    fn plane() -> Vec<(CubeTarget, CubeRect)> {
        vec![
            (CubeTarget { column: 0, row: 0, window: Some(1) }, cr(0, 0, 480, 1080)),
            (CubeTarget { column: 0, row: 0, window: Some(2) }, cr(480, 0, 480, 1080)),
            (CubeTarget { column: 1, row: 0, window: Some(3) }, cr(960, 0, 960, 1080)),
            (CubeTarget { column: 1, row: 1, window: Some(4) }, cr(960, 1080, 960, 1080)),
            (CubeTarget { column: 2, row: 0, window: Some(5) }, cr(1920, 0, 960, 1080)),
        ]
    }
    /// Origin rect for `window`, plus the plane with that window excluded --
    /// mirrors what find_target_by_direction does before delegating.
    fn from_window(window: u32) -> (CubeRect, Vec<(CubeTarget, CubeRect)>) {
        let plane = plane();
        let origin = plane.iter().find(|(t, _)| t.window == Some(window)).unwrap().1;
        let rest = plane.into_iter().filter(|(t, _)| t.window != Some(window)).collect();
        (origin, rest)
    }
    fn expect(window: u32, column: usize, row: usize) -> Option<CubeTarget> {
        Some(CubeTarget { column, row, window: Some(window) })
    }

    #[test]
    fn right_crosses_a_column_divide() {
        // win 2 is the rightmost leaf of column 0's group, so Right leaves the
        // group entirely and lands in column 1. win 4 is also ahead on x, and
        // only the beam test keeps the row below out of a horizontal move.
        let (origin, rest) = from_window(2);
        assert_eq!(nearest_in_direction(origin, &rest, Direction::Right), expect(3, 1, 0));
    }

    #[test]
    fn right_prefers_the_in_group_neighbour() {
        // Both win 2 (major 0) and win 3 (major 480) are ahead; nearest wins, so
        // within-group navigation takes precedence over crossing a divide with
        // no special-casing anywhere.
        let (origin, rest) = from_window(1);
        assert_eq!(nearest_in_direction(origin, &rest, Direction::Right), expect(2, 0, 0));
    }

    #[test]
    fn down_crosses_a_row_divide() {
        let (origin, rest) = from_window(3);
        assert_eq!(nearest_in_direction(origin, &rest, Direction::Down), expect(4, 1, 1));
    }

    #[test]
    fn vertical_motion_never_leaves_the_column() {
        // Column 0 has exactly one row, so Down from win 1 has nowhere to go --
        // win 4 sits below on the plane but in a different x band, and the beam
        // test rejects it. This is the carousel invariant.
        let (origin, rest) = from_window(1);
        assert_eq!(nearest_in_direction(origin, &rest, Direction::Down), None);
        assert_eq!(nearest_in_direction(origin, &rest, Direction::Up), None);
    }

    #[test]
    fn right_wraps_from_the_rightmost_window() {
        // Nothing ahead of win 5, so the wrap pass runs and must pick the
        // FURTHEST candidate behind (win 1), not the nearest (win 3).
        let (origin, rest) = from_window(5);
        assert_eq!(nearest_in_direction(origin, &rest, Direction::Right), expect(1, 0, 0));
    }

    #[test]
    fn left_wraps_from_the_leftmost_window() {
        let (origin, rest) = from_window(1);
        assert_eq!(nearest_in_direction(origin, &rest, Direction::Left), expect(5, 2, 0));
    }

    #[test]
    fn vertical_wrap_stays_inside_the_column() {
        // Column 1 has two rows. Down from the bottom one wraps to the top one,
        // and Up from the top wraps to the bottom -- never to another column,
        // because the beam confines the wrap pass just as it does the forward pass.
        let (bottom, rest) = from_window(4);
        assert_eq!(nearest_in_direction(bottom, &rest, Direction::Down), expect(3, 1, 0));
        let (top, rest) = from_window(3);
        assert_eq!(nearest_in_direction(top, &rest, Direction::Up), expect(4, 1, 1));
    }

    #[test]
    fn a_lone_window_has_nowhere_to_go() {
        for direction in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            assert_eq!(nearest_in_direction(cr(0, 0, 960, 1080), &[], direction), None);
        }
    }

    #[test]
    fn empty_slots_are_valid_destinations() {
        // An empty group contributes its whole band as one candidate carrying
        // window: None -- focus lands there so a spawn has somewhere to go.
        let candidates = vec![
            (CubeTarget { column: 1, row: 0, window: None }, cr(960, 0, 960, 1080)),
        ];
        assert_eq!(
            nearest_in_direction(cr(0, 0, 960, 1080), &candidates, Direction::Right),
            Some(CubeTarget { column: 1, row: 0, window: None }),
        );
    }

    #[test]
    fn windows_larger_than_the_origin_are_reachable() {
        // The beam test is interval intersection, not containment: a candidate
        // that spans far more than the origin overlaps it, and so does one that
        // only clips a corner of its span.
        let origin = cr(0, 400, 480, 200);
        let candidates = vec![
            (CubeTarget { column: 1, row: 0, window: Some(9) }, cr(480, 0, 480, 1080)),
        ];
        assert_eq!(nearest_in_direction(origin, &candidates, Direction::Right), expect(9, 1, 0));

        let partial = vec![
            (CubeTarget { column: 1, row: 0, window: Some(8) }, cr(480, 500, 480, 1080)),
        ];
        assert_eq!(nearest_in_direction(origin, &partial, Direction::Right), expect(8, 1, 0));
    }

    #[test]
    fn merely_touching_the_beam_does_not_count() {
        // Candidate starts exactly where the origin's span ends: they abut on
        // the cross axis with zero shared extent, so it is not a neighbour.
        let origin = cr(0, 0, 480, 540);
        let candidates = vec![
            (CubeTarget { column: 1, row: 0, window: Some(9) }, cr(480, 540, 480, 540)),
        ];
        assert_eq!(nearest_in_direction(origin, &candidates, Direction::Right), None);
    }

    #[test]
    fn ties_are_broken_deterministically() {
        // Two candidates, equal major distance AND equal minor distance -- one
        // straddling above the origin's centre line, one below. The column/row
        // tail of the sort key decides, so the answer cannot depend on the order
        // compute_cube happened to emit them in.
        let origin = cr(0, 0, 480, 1080);
        let above = (CubeTarget { column: 1, row: 0, window: Some(7) }, cr(480, -540, 480, 1080));
        let below = (CubeTarget { column: 1, row: 1, window: Some(8) }, cr(480, 540, 480, 1080));

        let forward = vec![above, below];
        let reversed = vec![below, above];
        assert_eq!(
            nearest_in_direction(origin, &forward, Direction::Right),
            nearest_in_direction(origin, &reversed, Direction::Right),
        );
        assert_eq!(nearest_in_direction(origin, &forward, Direction::Right), expect(7, 1, 0));
    }

    // ---- find_target_by_direction ----
    //
    // Same plane as above, now built from a real Monitor: column 0 is one group
    // split into win 1 | win 2, column 1 has win 3 over win 4, column 2 holds
    // win 5 off screen (visible_columns = 2).

    fn chart_monitor() -> Monitor {
        monitor_of(
            vec![
                column_of(vec![hsplit_group(1, 2)]),
                column_of(vec![leaf_group(3), leaf_group(4)]),
                column_of(vec![leaf_group(5)]),
            ],
            2,
        )
    }

    #[test]
    fn focus_crosses_a_column_divide() {
        let target = find_target_by_direction(&chart_monitor(), BOUNDS, Some(2), Direction::Right);
        assert_eq!(target, expect(3, 1, 0));
    }

    #[test]
    fn focus_stays_in_group_when_it_can() {
        let target = find_target_by_direction(&chart_monitor(), BOUNDS, Some(1), Direction::Right);
        assert_eq!(target, expect(2, 0, 0));
    }

    #[test]
    fn focus_crosses_a_row_divide() {
        let target = find_target_by_direction(&chart_monitor(), BOUNDS, Some(3), Direction::Down);
        assert_eq!(target, expect(4, 1, 1));
    }

    #[test]
    fn focus_reaches_an_off_screen_column() {
        // Column 2 is past visible_columns and renders nowhere, but it is on the
        // plane -- reveal_slot is what will bring it into view afterwards.
        let target = find_target_by_direction(&chart_monitor(), BOUNDS, Some(3), Direction::Right);
        assert_eq!(target, expect(5, 2, 0));
    }

    #[test]
    fn focus_wraps_at_the_right_edge() {
        let target = find_target_by_direction(&chart_monitor(), BOUNDS, Some(5), Direction::Right);
        assert_eq!(target, expect(1, 0, 0));
    }

    #[test]
    fn the_origin_is_never_its_own_target() {
        // One group, two windows: wrapping Left from win 1 must reach win 2, not
        // land back on win 1.
        let monitor = monitor_of(vec![column_of(vec![hsplit_group(1, 2)])], 1);
        assert_eq!(find_target_by_direction(&monitor, BOUNDS, Some(1), Direction::Left), expect(2, 0, 0));
        assert_eq!(find_target_by_direction(&monitor, BOUNDS, Some(1), Direction::Right), expect(2, 0, 0));
    }

    #[test]
    fn a_lone_window_has_no_target() {
        let monitor = monitor_of(vec![column_of(vec![leaf_group(1)])], 1);
        for direction in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            assert_eq!(find_target_by_direction(&monitor, BOUNDS, Some(1), direction), None);
        }
    }

    #[test]
    fn no_focused_window_falls_back_to_the_active_slot() {
        // Group::new seeds active_window to its first leaf, so the active slot
        // resolves to win 1 and Right lands on its group neighbour.
        let target = find_target_by_direction(&chart_monitor(), BOUNDS, None, Direction::Right);
        assert_eq!(target, expect(2, 0, 0));
    }

    #[test]
    fn a_focused_id_off_the_plane_falls_back_to_the_active_slot() {
        // Fullscreen windows sit outside the grid, so compute_cube never emits
        // them. Nav must still work rather than dying on the missing origin.
        let target = find_target_by_direction(&chart_monitor(), BOUNDS, Some(999), Direction::Right);
        assert_eq!(target, expect(2, 0, 0));
    }

    #[test]
    fn an_empty_active_slot_is_a_valid_origin() {
        // Active column 0 holds an empty group: active_window() is None, so the
        // fallback target carries window: None and matches the whole-band candidate.
        let monitor = monitor_of(
            vec![column_of(vec![Group::empty()]), column_of(vec![leaf_group(1)])],
            2,
        );
        let target = find_target_by_direction(&monitor, BOUNDS, None, Direction::Right);
        assert_eq!(target, expect(1, 1, 0));
    }

    #[test]
    fn focus_can_land_on_an_empty_group() {
        // The destination half of the same rule -- an empty group is somewhere
        // focus can go, so a subsequent spawn has a home.
        let monitor = monitor_of(
            vec![column_of(vec![leaf_group(1)]), column_of(vec![Group::empty()])],
            2,
        );
        let target = find_target_by_direction(&monitor, BOUNDS, Some(1), Direction::Right);
        assert_eq!(target, Some(CubeTarget { column: 1, row: 0, window: None }));
    }

    #[test]
    fn focus_can_land_on_an_empty_row() {
        let monitor = monitor_of(vec![column_of(vec![leaf_group(1), Group::empty()])], 1);
        let target = find_target_by_direction(&monitor, BOUNDS, Some(1), Direction::Down);
        assert_eq!(target, Some(CubeTarget { column: 0, row: 1, window: None }));
    }

    #[test]
    fn active_pointers_aimed_at_a_groupless_column_yield_nothing() {
        // compute_cube emits no slots for a column with no groups, so the
        // fallback target matches nothing and the whole call no-ops rather than
        // papering over the broken invariant.
        let monitor = monitor_of(vec![column_of(vec![]), column_of(vec![leaf_group(1)])], 2);
        assert_eq!(find_target_by_direction(&monitor, BOUNDS, None, Direction::Right), None);
    }
