    use super::*;

    fn leaf_group(id: u32) -> Group {
        Group::new(TilingNode::Leaf { window_id: id })
    }

    fn split_group(left: u32, right: u32) -> Group {
        use super::super::tiling::SplitDirection;
        Group::new(TilingNode::Split {
            split_direction: SplitDirection::Horizontal,
            split_ratio: 0.5,
            left_child: Box::new(TilingNode::Leaf { window_id: left }),
            right_child: Box::new(TilingNode::Leaf { window_id: right }),
        })
    }

    #[test]
    fn removing_last_window_empties_the_group() {
        // a group holding a single leaf: removing it must leave the group empty,
        // not leave the dead leaf in place. This is the RemoveMe backstop.
        let mut g = leaf_group(1);
        assert!(matches!(g.remove_window(1), RemoveResult::Removed));
        assert!(g.layout.is_none());
        assert_eq!(g.count_windows(), 0);
    }

    #[test]
    fn removing_one_of_two_windows_keeps_the_group_populated() {
        // group with split(1, 2): removing 2 collapses to bare leaf(1), group stays Some.
        let mut g = split_group(1, 2);
        assert!(matches!(g.remove_window(2), RemoveResult::Removed));
        assert!(g.layout.is_some());
        assert_eq!(g.count_windows(), 1);
    }

    #[test]
    fn removing_missing_window_reports_not_found() {
        let mut g = leaf_group(1);
        assert!(matches!(g.remove_window(99), RemoveResult::NotFound));
        assert!(g.layout.is_some());
        assert_eq!(g.count_windows(), 1);
    }

    #[test]
    fn removing_from_an_empty_group_reports_not_found() {
        let mut g = leaf_group(1);
        g.remove_window(1); // now empty
        assert!(matches!(g.remove_window(1), RemoveResult::NotFound));
        assert!(g.layout.is_none());
    }
    #[test]
    fn scroll_wraps_every_direction() {
        let mut col = Column::new(50);
        for i in 1..=3 {
            col.add_group(leaf_group(i));
        }
        col.scroll_column(-1);
        assert_eq!(col.active_row, 2);
        col.scroll_column(3);
        assert_eq!(col.active_row, 2);
        col.scroll_column(7);
        assert_eq!(col.active_row, 0);
        col.scroll_column(-7);
        assert_eq!(col.active_row, 2);
    }
    fn active_window(col: &Column) -> u32 {
        match &col.groups[col.active_row].layout {
            Some(TilingNode::Leaf { window_id }) => *window_id,
            _ => panic!("test columns are single leaves"),
        }
    }
    #[test]
    fn rotate_moves_active_groups_across_columns() {
        let mut mon = Monitor::new(1, 3);
        for i in 1..=3 { let mut c = Column::new(50); c.add_group(leaf_group(i)); mon.add_column(c); }
        mon.rotate_columns(-1);
        // trace your bubble on [1,2,3] to lock the direction, then assert all three:
        // swap(0,1) -> [2,1,3], swap(1,2) -> [2,3,1]   ==> a LEFT rotation
        assert_eq!(active_window(&mon.columns[0]), 2);
        assert_eq!(active_window(&mon.columns[1]), 3);
        assert_eq!(active_window(&mon.columns[2]), 1);
  }

    // ---- add_window ----
    fn empty_group() -> Group {
        Group { layout: None }
    }

    // is `id` present anywhere in the group's tree?
    fn has_window(g: &Group, id: u32) -> bool {
        g.layout.as_ref().is_some_and(|n| n.find_window(id).is_some())
    }

    #[test]
    fn add_to_empty_group_seeds_the_root_leaf() {
        // First window into an empty group becomes the bare root. focused_id is
        // irrelevant here (no tree to search), so a nonsense value must still work.
        let mut g = empty_group();
        g.add_window(SplitDirection::Horizontal, 1, 999);
        assert!(g.layout.is_some());
        assert_eq!(g.count_windows(), 1);
        assert!(has_window(&g, 1));
    }

    #[test]
    fn add_splits_the_focused_leaf() {
        // group holding leaf(1); add 2 focused on 1 -> both present, count 2.
        let mut g = leaf_group(1);
        g.add_window(SplitDirection::Horizontal, 2, 1);
        assert_eq!(g.count_windows(), 2);
        assert!(has_window(&g, 1));
        assert!(has_window(&g, 2));
    }

    #[test]
    fn add_splits_the_focused_leaf_deep_in_the_tree() {
        // split(1, 2); add 3 focused on 2 -> split(1, split(2, 3)). The unfocused
        // sibling (window 1) must be left structurally untouched at the root's left.
        let mut g = split_group(1, 2);
        g.add_window(SplitDirection::Horizontal, 3, 2);
        assert_eq!(g.count_windows(), 3);
        assert!(has_window(&g, 3));
        match g.layout.as_ref().unwrap() {
            TilingNode::Split { left_child, .. } => {
                assert!(matches!(**left_child, TilingNode::Leaf { window_id: 1 }));
            }
            _ => panic!("root should still be a split"),
        }
    }

    #[test]
    fn add_with_an_unfocused_target_splits_the_first_leaf() {
        // focused_id isn't in this group's tree -> fall back to splitting the
        // leftmost leaf rather than dropping. This is the old leak, now closed:
        // group holds leaf(1); add 2 focused on a nonexistent 99 -> both present.
        let mut g = leaf_group(1);
        g.add_window(SplitDirection::Horizontal, 2, 99);
        assert_eq!(g.count_windows(), 2);
        assert!(has_window(&g, 1));
        assert!(has_window(&g, 2));
    }

    #[test]
    fn add_with_no_focus_targets_the_leftmost_leaf() {
        // split(1, 2); spawn with no focus (id 0 is never a real window) -> the
        // new window splits the leftmost leaf (1), giving split(split(1, 3), 2).
        // The right sibling (2) must be left structurally untouched.
        let mut g = split_group(1, 2);
        g.add_window(SplitDirection::Horizontal, 3, 0);
        assert_eq!(g.count_windows(), 3);
        assert!(has_window(&g, 3));
        match g.layout.as_ref().unwrap() {
            TilingNode::Split { left_child, right_child, .. } => {
                assert!(matches!(**right_child, TilingNode::Leaf { window_id: 2 }));
                match &**left_child {
                    TilingNode::Split { left_child: ll, right_child: lr, .. } => {
                        assert!(matches!(**ll, TilingNode::Leaf { window_id: 1 }));
                        assert!(matches!(**lr, TilingNode::Leaf { window_id: 3 }));
                    }
                    _ => panic!("left child should have become split(1, 3)"),
                }
            }
            _ => panic!("root should be a split"),
        }
    }

    // ---- grow_active_column ----

    #[test]
    fn grow_appends_after_the_only_row_and_activates_the_new_group() {
        // Single-group column (active_row 0 == last row): grow adds a second
        // group and jumps active_row to it. The new group is empty; the existing
        // window is left untouched at row 0.
        let mut mon = Monitor::new(0, 1);
        let mut c = Column::new(0);
        c.add_group(leaf_group(1));
        mon.add_column(c);

        mon.grow_active_column();

        let col = &mon.columns[0];
        assert_eq!(col.groups.len(), 2);
        assert_eq!(col.active_row, 1);
        assert!(col.groups[1].layout.is_none(), "new group starts empty");
        assert!(has_window(&col.groups[0], 1), "existing window stays at row 0");
    }

    #[test]
    fn grow_from_a_non_last_row_inserts_in_place_not_at_the_end() {
        // Column [g1, g2, g3] with the middle row active. Grow must insert the
        // fresh group directly *after* the active row (row 2) and activate it --
        // not append after g3. g3 shifts down to row 3, untouched. This is the
        // positional-insert guarantee that scroll(1) relies on to be correct.
        let mut mon = Monitor::new(0, 1);
        let mut c = Column::new(0);
        for i in 1..=3 { c.add_group(leaf_group(i)); }
        mon.add_column(c);
        mon.columns[0].active_row = 1;

        mon.grow_active_column();

        let col = &mon.columns[0];
        assert_eq!(col.groups.len(), 4);
        assert_eq!(col.active_row, 2, "the new group is now active");
        assert!(col.groups[2].layout.is_none(), "row 2 is the fresh empty group");
        assert!(has_window(&col.groups[1], 2), "the grown-from group stays at row 1");
        assert!(has_window(&col.groups[3], 3), "the row below shifted down, not overwritten");
    }

    // ---- add_window threads the split direction through to the node ----
    // Every add_window test above uses Horizontal and never inspects the axis,
    // so a dropped or hardcoded direction here would go unnoticed. These pin it.
    fn group_axis(g: &Group) -> Option<SplitDirection> {
        match g.layout.as_ref()? {
            TilingNode::Split { split_direction, .. } => Some(*split_direction),
            _ => None,
        }
    }

    #[test]
    fn add_window_honors_a_vertical_split() {
        // leaf(1) + add 2 focused on 1, Vertical -> the group's root becomes a
        // Vertical split, proving the direction survives the grid->tiling hop.
        let mut g = leaf_group(1);
        g.add_window(SplitDirection::Vertical, 2, 1);
        assert_eq!(g.count_windows(), 2);
        assert!(matches!(group_axis(&g), Some(SplitDirection::Vertical)));
    }

    #[test]
    fn seeding_an_empty_group_ignores_direction_and_makes_no_split() {
        // The first window into an empty group is a bare leaf regardless of the
        // requested direction -- nothing to split yet, so no axis applies.
        let mut g = empty_group();
        g.add_window(SplitDirection::Vertical, 1, 0);
        assert!(matches!(g.layout.as_ref(), Some(TilingNode::Leaf { window_id: 1 })));
        assert!(group_axis(&g).is_none());
    }

    // ---- directional move across the group grid ----
    // Build a monitor from a column spec: each inner slice lists the window id
    // in each group's single leaf (rows top-to-bottom); `active_rows` seeds each
    // column's cursor. Enough to exercise ragged-grid row selection. Columns
    // here are always non-empty -- the empty-group cases are built by hand below.
    fn monitor_from(columns: &[&[u32]], active_rows: &[usize]) -> Monitor {
        let mut mon = Monitor::new(0, columns.len().max(1));
        for (ci, ids) in columns.iter().enumerate() {
            let mut col = Column::new(0);
            for &id in ids.iter() {
                col.add_group(leaf_group(id));
            }
            col.active_row = active_rows[ci];
            mon.add_column(col);
        }
        mon
    }

    #[test]
    fn find_column_and_row_locates_a_window_in_a_ragged_grid() {
        // window 4 lives in column 1, row 2 -- the finder must return exactly that.
        let mon = monitor_from(&[&[1], &[2, 3, 4]], &[0, 0]);
        assert_eq!(mon.find_column_and_row_by_window_id(4), Some((1, 2)));
        assert_eq!(mon.find_column_and_row_by_window_id(1), Some((0, 0)));
        assert_eq!(mon.find_column_and_row_by_window_id(99), None);
    }

    #[test]
    fn direction_up_down_walk_rows_within_the_column() {
        // single column [1,2,3]; window 2 (row 1) resolves to row 0 up / row 2 down.
        let mon = monitor_from(&[&[1, 2, 3]], &[0]);
        assert_eq!(mon.find_group_by_direction(2, Direction::Up), Some((0, 0)));
        assert_eq!(mon.find_group_by_direction(2, Direction::Down), Some((0, 2)));
    }

    #[test]
    fn vertical_moves_no_op_at_the_column_edges() {
        // top row can't go up, bottom row can't go down -- both None, no wrap.
        let mon = monitor_from(&[&[1, 2, 3]], &[0]);
        assert_eq!(mon.find_group_by_direction(1, Direction::Up), None);
        assert_eq!(mon.find_group_by_direction(3, Direction::Down), None);
    }

    #[test]
    fn horizontal_moves_no_op_at_the_outer_columns() {
        // lone column: nothing to the left or right of window 1.
        let mon = monitor_from(&[&[1]], &[0]);
        assert_eq!(mon.find_group_by_direction(1, Direction::Left), None);
        assert_eq!(mon.find_group_by_direction(1, Direction::Right), None);
    }

    #[test]
    fn horizontal_move_lands_in_the_destination_columns_active_row() {
        // The ragged-grid contract: moving across columns ignores the *source*
        // row and targets the destination column's active_row. Column 1's cursor
        // sits on row 2, so Right from column 0 lands at (1, 2) -- not (1, 0),
        // and not a clamp of the source row.
        let mon = monitor_from(&[&[1], &[2, 3, 4]], &[0, 2]);
        assert_eq!(mon.find_group_by_direction(1, Direction::Right), Some((1, 2)));
        // Left from the deep column resolves against the shallow column's
        // active_row (0), which is in-bounds even though source row 2 has no
        // counterpart in column 0.
        assert_eq!(mon.find_group_by_direction(4, Direction::Left), Some((0, 0)));
    }

    #[test]
    fn find_group_by_direction_reports_none_for_a_missing_window() {
        let mon = monitor_from(&[&[1, 2]], &[0]);
        assert_eq!(mon.find_group_by_direction(99, Direction::Down), None);
    }

    #[test]
    fn move_window_into_an_empty_group_relocates_and_moves_the_cursor() {
        // column 0: [leaf(1)]; column 1: [empty group]. Move 1 into (1, 0).
        let mut mon = Monitor::new(0, 2);
        let mut c0 = Column::new(0);
        c0.add_group(leaf_group(1));
        mon.add_column(c0);
        let mut c1 = Column::new(0);
        c1.add_group(empty_group());
        mon.add_column(c1);

        mon.move_window_to_group(1, 1, 0, SplitDirection::Horizontal);

        assert!(!has_window(&mon.columns[0].groups[0], 1), "source no longer holds it");
        assert!(mon.columns[0].groups[0].layout.is_none(), "emptied source is kept, not pruned");
        assert!(has_window(&mon.columns[1].groups[0], 1), "landed in the destination group");
        assert!(
            matches!(mon.columns[1].groups[0].layout.as_ref(), Some(TilingNode::Leaf { window_id: 1 })),
            "seeding an empty group makes a bare leaf, no split"
        );
        assert_eq!(mon.active_column, 1, "cursor followed to the destination column");
        assert_eq!(mon.columns[1].active_row, 0, "cursor followed to the destination row");
    }

    #[test]
    fn move_window_into_a_populated_group_splits_along_the_given_axis() {
        // column 0: [leaf(1)]; column 1: [leaf(2)]. Move 1 into (1, 0) Vertical:
        // the destination leaf(2) splits vertically to hold both windows, and the
        // axis is exactly the one handed to move_window_to_group (policy decides
        // it upstream; the model only threads it through).
        let mut mon = Monitor::new(0, 2);
        let mut c0 = Column::new(0);
        c0.add_group(leaf_group(1));
        mon.add_column(c0);
        let mut c1 = Column::new(0);
        c1.add_group(leaf_group(2));
        mon.add_column(c1);

        mon.move_window_to_group(1, 1, 0, SplitDirection::Vertical);

        let dest = &mon.columns[1].groups[0];
        assert!(has_window(dest, 1));
        assert!(has_window(dest, 2));
        assert_eq!(dest.count_windows(), 2);
        assert!(matches!(group_axis(dest), Some(SplitDirection::Vertical)));
    }
