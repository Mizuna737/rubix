use super::tiling::{TilingNode, SplitDirection};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub fn compute_layout(node: &TilingNode, bounds: Rect) -> Vec<(u32, Rect)> {
    match node {
        TilingNode::Leaf { window_id } => {
            vec![(*window_id,bounds)]
        }
        TilingNode::Split { split_direction, split_ratio, left_child, right_child } => {
            let (left_bounds, right_bounds) = match split_direction {
                SplitDirection::Horizontal => {
                    let left = Rect {
                        x: bounds.x,
                        y: bounds.y,
                        width: (bounds.width as f32 * *split_ratio) as u32,
                        height: bounds.height,
                    };
                    let right = Rect {
                        x: bounds.x + left.width,
                        y: bounds.y,
                        width: bounds.width - left.width,
                        height: bounds.height,
                    };
                    (left,right)
                },
                SplitDirection::Vertical => {
                    let left = Rect {
                        x: bounds.x,
                        y: bounds.y,
                        width: bounds.width,
                        height: (bounds.height as f32 * *split_ratio) as u32,
                    };
                    let right = Rect {
                        x: bounds.x,
                        y: bounds.y + left.height,
                        width: bounds.width,
                        height: bounds.height - left.height,
                    };
                    (left,right)
                }
            };
            let mut windows = compute_layout(left_child,left_bounds);
            windows.extend(compute_layout(right_child,right_bounds));
            windows
        }
    }
}

#[cfg(test)]
mod tests {
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
        let layout = compute_layout(&leaf(1), rect(0, 0, 800, 600));
        assert_eq!(layout, vec![(1, rect(0, 0, 800, 600))]);
    }

    #[test]
    fn horizontal_split_places_children_side_by_side_and_tiles_exactly() {
        let tree = hsplit(0.5, leaf(1), leaf(2));
        let layout = compute_layout(&tree, rect(0, 0, 800, 600));
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
        let layout = compute_layout(&tree, rect(0, 0, 800, 600));
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
        let layout = compute_layout(&tree, rect(0, 0, 100, 50));
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
        let layout = compute_layout(&tree, rect(0, 0, 800, 600));

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
}
