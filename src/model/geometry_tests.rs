    use super::*;

    // ---- builders ----
    fn leaf(id: u32) -> TilingNode {
        TilingNode::Leaf { window_id: id }
    }
    fn hsplit(ratio: f32, left: TilingNode, right: TilingNode) -> TilingNode {
        TilingNode::Split {
            split_direction: SplitDirection::Horizontal,
            split_ratio: ratio,
            left_child: Box::new(left),
            right_child: Box::new(right),
        }
    }
    fn vsplit(ratio: f32, left: TilingNode, right: TilingNode) -> TilingNode {
        TilingNode::Split {
            split_direction: SplitDirection::Vertical,
            split_ratio: ratio,
            left_child: Box::new(left),
            right_child: Box::new(right),
        }
    }
    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }
    // rect for a given window id in a layout
    fn find(layout: &[(u32, Rect)], id: u32) -> Rect {
        layout.iter().find(|(i, _)| *i == id).expect("window id present in layout").1
    }
    // sum of all rect areas -- for exact-tiling checks
    fn covered_area(layout: &[(u32, Rect)]) -> u64 {
        layout.iter().map(|(_, r)| r.width as u64 * r.height as u64).sum()
    }

    #[test]
    fn single_leaf_fills_the_whole_bounds() {
        let layout = compute_layout(&leaf(1), rect(0, 0, 800, 600), 0);
        assert_eq!(layout, vec![(1, rect(0, 0, 800, 600))]);
    }

    #[test]
    fn horizontal_split_places_children_side_by_side_and_tiles_exactly() {
        let tree = hsplit(0.5, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 800, 600), 0);
        assert_eq!(find(&layout, 1), rect(0, 0, 400, 600));
        assert_eq!(find(&layout, 2), rect(400, 0, 400, 600));
        // right child begins exactly where left ends -- no gap, no overlap
        let (l, r) = (find(&layout, 1), find(&layout, 2));
        assert_eq!(r.x, l.x + l.width);
        assert_eq!(l.width + r.width, 800);
    }

    #[test]
    fn vertical_split_stacks_children_and_tiles_exactly() {
        let tree = vsplit(0.5, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 800, 600), 0);
        assert_eq!(find(&layout, 1), rect(0, 0, 800, 300));
        assert_eq!(find(&layout, 2), rect(0, 300, 800, 300));
        let (top, bottom) = (find(&layout, 1), find(&layout, 2));
        assert_eq!(bottom.y, top.y + top.height);
        assert_eq!(top.height + bottom.height, 600);
    }

    #[test]
    fn uneven_ratio_leaves_no_gap_via_remainder() {
        // 0.333 * 100 = 33 (truncated); right must get the remaining 67, not 66.
        let tree = hsplit(0.333, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 100, 50), 0);
        let (l, r) = (find(&layout, 1), find(&layout, 2));
        assert_eq!(l.width, 33);
        assert_eq!(r.width, 67);
        assert_eq!(r.x, 33);
        assert_eq!(l.width + r.width, 100); // exact, no seam
    }

    #[test]
    fn nested_three_window_layout_a_b_c() {
        // Root H-split: A | (B/C column). Right column V-split: B over C.
        //   +-----+-----+
        //   |  A  |  B  |
        //   |     +-----+
        //   |  A  |  C  |
        //   +-----+-----+
        let tree = hsplit(0.5, leaf(1), vsplit(0.5, leaf(2), leaf(3)));
        let layout = compute_layout(&tree, rect(0, 0, 800, 600), 0);

        let a = find(&layout, 1);
        let b = find(&layout, 2);
        let c = find(&layout, 3);

        assert_eq!(a, rect(0, 0, 400, 600));
        assert_eq!(b, rect(400, 0, 400, 300));
        assert_eq!(c, rect(400, 300, 400, 300));

        // C's left edge is the root divider -- the same line as A's right edge
        // and B's left edge (the shared column boundary).
        assert_eq!(c.x, a.x + a.width);
        assert_eq!(c.x, b.x);

        // three leaves, and they tile the output with zero uncovered area
        assert_eq!(layout.len(), 3);
        assert_eq!(covered_area(&layout), 800 * 600);
    }

    #[test]
    fn uneven_vertical_ratio_leaves_no_gap_via_remainder() {
        // Vertical analog of the horizontal remainder test: 0.333 * 50 = 16
        // (truncated), so the bottom child must take the remaining 34, not 33.
        let tree = vsplit(0.333, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 100, 50), 0);
        let (top, bottom) = (find(&layout, 1), find(&layout, 2));
        assert_eq!(top.height, 16);
        assert_eq!(bottom.height, 34);
        assert_eq!(bottom.y, 16); // bottom begins exactly where top ends
        assert_eq!(top.height + bottom.height, 50); // exact, no seam
    }

    // ---- inner_gap ----
    // Every split node inserts exactly one inner_gap in its seam; the ratio applies
    // to the *usable* span (bounds minus the gap), and the cross axis is untouched.

    #[test]
    fn horizontal_split_inserts_exactly_one_inner_gap() {
        // usable = 800 - 20 = 780; 0.5 * 780 = 390 each, 20px seam between them.
        let tree = hsplit(0.5, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 800, 600), 20);
        let (l, r) = (find(&layout, 1), find(&layout, 2));
        assert_eq!(l, rect(0, 0, 390, 600));
        assert_eq!(r, rect(410, 0, 390, 600));
        // one inner_gap in the seam, and content + gap still spans the bounds exactly
        assert_eq!(r.x - (l.x + l.width), 20);
        assert_eq!(l.width + 20 + r.width, 800);
        // cross axis (height) passes through untouched -- the gap is axis-local
        assert_eq!(l.height, 600);
        assert_eq!(r.height, 600);
    }

    #[test]
    fn vertical_split_inserts_exactly_one_inner_gap() {
        // usable = 600 - 20 = 580; 0.5 * 580 = 290 each, 20px seam between them.
        let tree = vsplit(0.5, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 800, 600), 20);
        let (top, bottom) = (find(&layout, 1), find(&layout, 2));
        assert_eq!(top, rect(0, 0, 800, 290));
        assert_eq!(bottom, rect(0, 310, 800, 290));
        assert_eq!(bottom.y - (top.y + top.height), 20);
        assert_eq!(top.height + 20 + bottom.height, 600);
        // cross axis (width) untouched
        assert_eq!(top.width, 800);
        assert_eq!(bottom.width, 800);
    }

    #[test]
    fn nested_split_gaps_are_one_per_seam_and_do_not_accumulate() {
        // A | (B / C): a root H-split whose right child is a V-split. Each seam --
        // the A|column divider and the B/C divider -- must carry exactly one gap,
        // and neither should bleed onto the other's axis.
        let tree = hsplit(0.5, leaf(1), vsplit(0.5, leaf(2), leaf(3)));
        let layout = compute_layout(&tree, rect(0, 0, 800, 600), 20);
        let (a, b, c) = (find(&layout, 1), find(&layout, 2), find(&layout, 3));

        // root H-split: usable 780, A gets 390, right column starts at 410 (one gap)
        assert_eq!(a, rect(0, 0, 390, 600));
        assert_eq!(b, rect(410, 0, 390, 290));
        assert_eq!(c, rect(410, 310, 390, 290));

        // horizontal seam A|column = one inner_gap; B and C share the column's left edge
        assert_eq!(b.x - (a.x + a.width), 20);
        assert_eq!(c.x, b.x);
        // vertical seam B/C = one inner_gap, independent of the horizontal one
        assert_eq!(c.y - (b.y + b.height), 20);
        // B and C keep the full right-column width -- the V-split's gap didn't touch x
        assert_eq!(b.width, 390);
        assert_eq!(c.width, 390);
    }

    #[test]
    fn inner_gap_larger_than_bounds_saturates_to_zero_size_without_panic() {
        // Pathological: gap exceeds the axis. usable saturates to 0, so both
        // children collapse to zero width -- degenerate but panic-free (the u32
        // subtractions never underflow).
        let tree = hsplit(0.5, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 30, 50), 100);
        assert_eq!(find(&layout, 1).width, 0);
        assert_eq!(find(&layout, 2).width, 0);
    }

    #[test]
    fn single_leaf_ignores_inner_gap() {
        // A leaf has no seam, so a nonzero gap must not inset it at all.
        let layout = compute_layout(&leaf(1), rect(0, 0, 800, 600), 20);
        assert_eq!(layout, vec![(1, rect(0, 0, 800, 600))]);
    }

    // ---- longer_axis (auto-split heuristic) ----
    #[test]
    fn longer_axis_picks_horizontal_for_a_wide_rect() {
        // wider than tall -> split side-by-side, keeping cells squarish
        assert!(matches!(rect(0, 0, 800, 300).longer_axis(), SplitDirection::Horizontal));
    }

    #[test]
    fn longer_axis_picks_vertical_for_a_tall_rect() {
        assert!(matches!(rect(0, 0, 300, 800).longer_axis(), SplitDirection::Vertical));
    }

    #[test]
    fn longer_axis_breaks_a_square_tie_toward_horizontal() {
        // exactly square: the `>=` tie-break must land on Horizontal, not Vertical
        assert!(matches!(rect(0, 0, 500, 500).longer_axis(), SplitDirection::Horizontal));
    }
