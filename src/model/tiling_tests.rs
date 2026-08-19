    use super::*;

    // ---- builders ----
    fn leaf(id: u32) -> TilingNode {
        TilingNode::Leaf { window_id: id }
    }

    fn split(left: TilingNode, right: TilingNode) -> TilingNode {
        TilingNode::Split {
            split_direction: SplitDirection::Horizontal,
            split_ratio: 0.5,
            left_child: Box::new(left),
            right_child: Box::new(right),
        }
    }

    // ---- inspectors ----
    // window_id if this node is a leaf, else None.
    fn leaf_id(node: &TilingNode) -> Option<u32> {
        match node {
            TilingNode::Leaf { window_id } => Some(*window_id),
            _ => None,
        }
    }

    // (left_id, right_id) if node is a Split whose children are both leaves.
    fn split_leaf_ids(node: &TilingNode) -> Option<(u32, u32)> {
        match node {
            TilingNode::Split { left_child, right_child, .. } => {
                Some((leaf_id(left_child)?, leaf_id(right_child)?))
            }
            _ => None,
        }
    }

    // Independent recursive window count -- deliberately does NOT use the
    // CountWindows impl, so a bug there can't hide a bug here.
    fn count(node: &TilingNode) -> usize {
        match node {
            TilingNode::Leaf { .. } => 1,
            TilingNode::Split { left_child, right_child, .. } => {
                count(left_child) + count(right_child)
            }
        }
    }

    // ---- find_window ----
    #[test]
    fn find_window_hits_and_misses_in_a_leaf() {
        let tree = leaf(7);
        assert_eq!(leaf_id(tree.find_window(7).expect("should find 7")), Some(7));
        assert!(tree.find_window(8).is_none());
    }

    #[test]
    fn find_window_locates_leaves_at_any_depth() {
        // split(1, split(2, 3))
        let tree = split(leaf(1), split(leaf(2), leaf(3)));
        assert_eq!(leaf_id(tree.find_window(1).expect("1")), Some(1));
        assert_eq!(leaf_id(tree.find_window(2).expect("2")), Some(2));
        assert_eq!(leaf_id(tree.find_window(3).expect("3")), Some(3));
        assert!(tree.find_window(99).is_none());
    }

    // ---- find_window_mut ----
    #[test]
    fn find_window_mut_allows_mutation_through_the_reference() {
        let mut tree = split(leaf(1), leaf(2));
        // rename window 2 -> 42 via the returned &mut, proving it's a live handle
        match tree.find_window_mut(2) {
            Some(TilingNode::Leaf { window_id }) => *window_id = 42,
            _ => panic!("expected to find leaf 2"),
        }
        assert!(tree.find_window(2).is_none());
        assert_eq!(leaf_id(tree.find_window(42).expect("42")), Some(42));
    }

    #[test]
    fn find_window_mut_misses_return_none() {
        let mut tree = split(leaf(1), leaf(2));
        assert!(tree.find_window_mut(3).is_none());
    }

    // ---- find_first_leaf_mut ----
    #[test]
    fn find_first_leaf_mut_on_a_bare_leaf_returns_that_leaf() {
        let mut tree = leaf(5);
        assert_eq!(leaf_id(tree.find_first_leaf_mut()), Some(5));
    }

    #[test]
    fn find_first_leaf_mut_descends_left_regardless_of_ids() {
        // split(split(9, 2), 3): leftmost leaf is 9, though it's the largest id.
        // Resolution is positional (recurse left_child), never by value.
        let mut tree = split(split(leaf(9), leaf(2)), leaf(3));
        assert_eq!(leaf_id(tree.find_first_leaf_mut()), Some(9));
    }

    #[test]
    fn find_first_leaf_mut_allows_mutation_through_the_reference() {
        // the returned &mut is a live handle: rename the leftmost leaf, see it stick.
        let mut tree = split(leaf(1), leaf(2));
        match tree.find_first_leaf_mut() {
            TilingNode::Leaf { window_id } => *window_id = 7,
            _ => panic!("leftmost node should be a leaf"),
        }
        assert_eq!(split_leaf_ids(&tree), Some((7, 2)));
    }

    // ---- split_window ----
    #[test]
    fn split_window_turns_a_leaf_into_a_split_old_left_new_right() {
        let mut node = leaf(1);
        TilingNode::split_window(&mut node, SplitDirection::Vertical, 2);
        // convention: existing window stays left, new window goes right
        assert_eq!(split_leaf_ids(&node), Some((1, 2)));
        assert_eq!(count(&node), 2);
    }

    #[test]
    fn split_window_splits_a_leaf_deep_in_the_tree() {
        // split(1, 2); split window 2 into (2, 3)
        let mut tree = split(leaf(1), leaf(2));
        {
            let target = tree.find_window_mut(2).expect("window 2 exists");
            TilingNode::split_window(target, SplitDirection::Horizontal, 3);
        }
        // tree is now split(1, split(2, 3)) -- window 1 untouched
        assert_eq!(count(&tree), 3);
        assert_eq!(leaf_id(tree.find_window(1).expect("1")), Some(1));
        assert!(tree.find_window(2).is_some());
        assert!(tree.find_window(3).is_some());
        match &tree {
            TilingNode::Split { left_child, right_child, .. } => {
                assert_eq!(leaf_id(left_child), Some(1));
                assert_eq!(split_leaf_ids(right_child), Some((2, 3)));
            }
            _ => panic!("root should still be a split"),
        }
    }

    #[test]
    #[should_panic(expected = "split_window called on a split!")]
    fn split_window_on_a_split_panics() {
        let mut node = split(leaf(1), leaf(2));
        TilingNode::split_window(&mut node, SplitDirection::Vertical, 3);
    }

    // The survivor id carried by Removed; None for RemoveMe / NotFound.
    fn survivor(result: RemoveResult) -> Option<u32> {
        match result {
            RemoveResult::Removed { survivor_id } => Some(survivor_id),
            _ => None,
        }
    }

    // ---- remove_window ----
    #[test]
    fn remove_collapses_parent_into_the_surviving_left_sibling() {
        // split(1, 2), remove 2 -> node becomes bare leaf(1)
        let mut tree = split(leaf(1), leaf(2));
        assert_eq!(survivor(tree.remove_window(2)), Some(1));
        assert_eq!(leaf_id(&tree), Some(1));
        assert_eq!(count(&tree), 1);
    }

    #[test]
    fn remove_collapses_parent_into_the_surviving_right_sibling() {
        // split(1, 2), remove 1 -> node becomes bare leaf(2)
        let mut tree = split(leaf(1), leaf(2));
        assert_eq!(survivor(tree.remove_window(1)), Some(2));
        assert_eq!(leaf_id(&tree), Some(2));
        assert_eq!(count(&tree), 1);
    }

    #[test]
    fn remove_deep_leaf_collapses_only_its_own_parent() {
        // split(1, split(2, 3)), remove 2 -> split(1, 3); everything above is untouched
        let mut tree = split(leaf(1), split(leaf(2), leaf(3)));
        assert_eq!(survivor(tree.remove_window(2)), Some(3));
        assert_eq!(count(&tree), 2);
        assert!(tree.find_window(2).is_none());
        assert_eq!(split_leaf_ids(&tree), Some((1, 3)));
    }

    #[test]
    fn remove_from_a_left_subtree_collapses_correctly() {
        // split(split(1, 2), 3), remove 1 -> split(2, 3)
        let mut tree = split(split(leaf(1), leaf(2)), leaf(3));
        assert_eq!(survivor(tree.remove_window(1)), Some(2));
        assert_eq!(count(&tree), 2);
        assert_eq!(split_leaf_ids(&tree), Some((2, 3)));
    }

    #[test]
    fn remove_the_only_window_signals_remove_me_and_leaves_the_node_intact() {
        // a bare leaf has no parent to collapse it, so it reports RemoveMe upward;
        // emptying the group is the caller's job, so the node is unchanged here.
        let mut tree = leaf(1);
        assert!(matches!(tree.remove_window(1), RemoveResult::RemoveMe));
        assert_eq!(leaf_id(&tree), Some(1));
    }

    #[test]
    fn remove_reports_the_local_survivor_not_the_trees_first_leaf() {
        // split(split(1, 2), split(3, 4)); removing 3 collapses only the right
        // pair, so the survivor is its sibling, 4. This is the case that makes
        // threading the id up the recursion worth doing at all -- answering with
        // the whole tree's first leaf would say 1, on the far side of the group.
        let mut tree = split(split(leaf(1), leaf(2)), split(leaf(3), leaf(4)));
        assert_eq!(survivor(tree.remove_window(3)), Some(4));
        assert_eq!(count(&tree), 3);
        assert!(tree.find_window(3).is_none());
    }

    #[test]
    fn remove_reports_the_promoted_subtrees_first_leaf() {
        // split(1, split(2, 3)); removing 1 promotes the entire right subtree,
        // which has no single surviving window -- its first leaf, 2, is the answer.
        let mut tree = split(leaf(1), split(leaf(2), leaf(3)));
        assert_eq!(survivor(tree.remove_window(1)), Some(2));
        assert_eq!(count(&tree), 2);
    }

    #[test]
    fn remove_missing_window_reports_not_found_and_changes_nothing() {
        let mut tree = split(leaf(1), leaf(2));
        assert!(matches!(tree.remove_window(99), RemoveResult::NotFound));
        assert_eq!(count(&tree), 2);
        assert_eq!(split_leaf_ids(&tree), Some((1, 2)));
    }

    #[test]
    fn remove_missing_from_a_bare_leaf_reports_not_found() {
        let mut tree = leaf(1);
        assert!(matches!(tree.remove_window(2), RemoveResult::NotFound));
        assert_eq!(leaf_id(&tree), Some(1));
    }

    // ---- split direction is recorded, not hardcoded ----
    // SplitDirection has no PartialEq, so read it out by copy and match on it.
    fn split_axis(node: &TilingNode) -> Option<SplitDirection> {
        match node {
            TilingNode::Split { split_direction, .. } => Some(*split_direction),
            _ => None,
        }
    }

    #[test]
    fn split_window_records_a_vertical_split() {
        let mut node = leaf(1);
        TilingNode::split_window(&mut node, SplitDirection::Vertical, 2);
        assert!(matches!(split_axis(&node), Some(SplitDirection::Vertical)));
    }

    #[test]
    fn split_window_records_a_horizontal_split() {
        // Guards the other way: a hardcode to Vertical would fail this one.
        let mut node = leaf(1);
        TilingNode::split_window(&mut node, SplitDirection::Horizontal, 2);
        assert!(matches!(split_axis(&node), Some(SplitDirection::Horizontal)));
    }

    #[test]
    fn split_direction_is_per_node_not_global() {
        // Root split(1, 2) is Horizontal (the `split` helper); then split leaf 2
        // vertically. The root must stay Horizontal while the new inner node is
        // Vertical -- proving direction lives on each Split, not one shared axis.
        let mut tree = split(leaf(1), leaf(2));
        {
            let target = tree.find_window_mut(2).expect("window 2 exists");
            TilingNode::split_window(target, SplitDirection::Vertical, 3);
        }
        assert!(matches!(split_axis(&tree), Some(SplitDirection::Horizontal)));
        match &tree {
            TilingNode::Split { right_child, .. } => {
                assert!(matches!(split_axis(right_child), Some(SplitDirection::Vertical)));
            }
            _ => panic!("root should still be a split"),
        }
    }

    // ---- SplitDirection::toggled ----
    #[test]
    fn toggled_swaps_the_axis_both_ways() {
        assert!(matches!(SplitDirection::Horizontal.toggled(), SplitDirection::Vertical));
        assert!(matches!(SplitDirection::Vertical.toggled(), SplitDirection::Horizontal));
    }

    // ---- flip_parent_split_direction ----
    #[test]
    fn flip_toggles_the_immediate_parents_axis_both_ways() {
        // split(1, 2) is Horizontal; flipping a child flips that split, and
        // flipping again returns it -- proving the toggle is actually stored.
        let mut tree = split(leaf(1), leaf(2));
        assert!(tree.flip_parent_split_direction(2));
        assert!(matches!(split_axis(&tree), Some(SplitDirection::Vertical)));
        assert!(tree.flip_parent_split_direction(1));
        assert!(matches!(split_axis(&tree), Some(SplitDirection::Horizontal)));
    }

    #[test]
    fn flip_touches_only_the_immediate_parent_not_ancestors() {
        // split(1, split(2, 3)); both splits start Horizontal. Flipping window 2
        // must flip ONLY the inner split (2's direct parent). The root -- an
        // ancestor, not the parent -- stays Horizontal. This is the bubble-up bug.
        let mut tree = split(leaf(1), split(leaf(2), leaf(3)));
        assert!(tree.flip_parent_split_direction(2));
        assert!(matches!(split_axis(&tree), Some(SplitDirection::Horizontal)), "root untouched");
        match &tree {
            TilingNode::Split { right_child, .. } => {
                assert!(matches!(split_axis(right_child), Some(SplitDirection::Vertical)), "inner flipped");
            }
            _ => panic!("root should still be a split"),
        }
    }

    #[test]
    fn flip_on_a_lone_leaf_finds_it_but_changes_nothing() {
        // "Found" contract: locating the window reports true even on a bare leaf.
        // There's no parent split, so nothing flips -- the tree is left untouched.
        let mut tree = leaf(1);
        assert!(tree.flip_parent_split_direction(1)); // found
        assert!(matches!(&tree, TilingNode::Leaf { window_id: 1 })); // but unchanged
    }

    #[test]
    fn flip_a_missing_window_reports_false_and_changes_nothing() {
        let mut tree = split(leaf(1), leaf(2));
        assert!(!tree.flip_parent_split_direction(99));
        assert!(matches!(split_axis(&tree), Some(SplitDirection::Horizontal)));
    }
