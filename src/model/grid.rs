use super::tiling::{TilingNode, SplitDirection, RemoveResult};
use super::traits::{CountWindows, sum_windows};
pub struct Group {
    pub layout: Option<TilingNode>,
}

impl Group {
    pub fn new( layout: TilingNode ) -> Self {
        Group {
            layout: Some(layout)
        }
    }
    pub fn remove_window(&mut self, window_id: u32) -> RemoveResult {
        let result = match &mut self.layout {
            Some(node) => node.remove_window(window_id),
            None => return RemoveResult::NotFound,
        };

        match result {
            RemoveResult::Removed => RemoveResult::Removed,
            RemoveResult::NotFound => RemoveResult::NotFound,
            RemoveResult::RemoveMe => {
                self.layout = None;
                RemoveResult::Removed
            }
        }
    }
    pub fn add_window(&mut self, direction: SplitDirection, window_id: u32, focused_id: u32) {
        match &mut self.layout {
            Some(root_node) => {
                let focused_node = root_node.find_window_mut(focused_id);
                match focused_node {
                    None => (), // TODO: handle add_window in unfocused group. Currently leaks.
                    Some(focused_node) => {
                        TilingNode::split_window(focused_node, direction, window_id)
                    }
                }
            },
            None => {
                self.layout = Some(TilingNode::new(window_id));
            },
        };
    }
}

impl CountWindows for Group {
    fn count_windows(&self) -> usize {
        match &self.layout {
            Some(node) => node.count_windows(),
            None => 0
        }
    }
}

pub struct Column {
    width: u32,
    active_row: usize,
    groups: Vec<Group>
}

impl Column {
    pub fn new(width: u32) -> Self {
        Column {
            width,
            active_row: 0,
            groups: Vec::new(),
        }
    }
    pub fn add_group(&mut self, group: Group) {
        self.groups.push(group)
    }
    pub fn scroll_column(&mut self, motion: isize) {
        debug_assert!(!self.groups.is_empty(), "scroll_column on empty column");
        let rows = self.groups.len() as isize;
        let signed_active_row = self.active_row as isize;
        self.active_row = (signed_active_row + motion).rem_euclid(rows) as usize
    }
}

impl CountWindows for Column {
    fn count_windows(&self) -> usize {
        sum_windows(&self.groups)
    }
}
pub struct Monitor {
    id: u32,
    visible_columns: usize,
    columns: Vec<Column>
}

impl Monitor {
    pub fn new(id: u32, visible_columns: usize) -> Self {
        Monitor {
            id,
            visible_columns,
            columns: Vec::new(),
        }
    }

    pub fn add_column(&mut self, column: Column) {
        self.columns.push(column)
    }

    fn rotate_columns(&mut self, motion: isize) { // Positive motion rotates right, negative
                                                  // rotates left.
        let k = motion.rem_euclid(self.columns.len() as isize);
        for _i in 0..k {
           for j in 0..self.columns.len()-1 {
               let (column_a,column_b) = self.columns.split_at_mut(j+1);
               let active_row_a = column_a[0].active_row;
               let active_row_b = column_b[0].active_row;
               std::mem::swap(&mut column_a[0].groups[active_row_a],&mut column_b[0].groups[active_row_b]);
           }
        }
    }
}

impl CountWindows for Monitor {
    fn count_windows(&self) -> usize {
        sum_windows(&self.columns)
    }
}

#[cfg(test)]
mod tests {
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
    fn add_with_an_unfocused_target_is_dropped() {
        // focused_id isn't in this group's tree -> the None arm fires and the new
        // window is silently dropped. Documents the known leak (see the TODO on
        // add_window); update this test when that branch learns to place it.
        let mut g = leaf_group(1);
        g.add_window(SplitDirection::Horizontal, 2, 99);
        assert_eq!(g.count_windows(), 1);
        assert!(has_window(&g, 1));
        assert!(!has_window(&g, 2));
    }
}
