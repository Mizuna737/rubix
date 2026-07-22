use super::tiling::{TilingNode, SplitDirection, RemoveResult};
use super::traits::{CountWindows, sum_windows};
use super::geometry::{compute_layout, Rect};
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
    pub fn try_add_window(&mut self, direction: SplitDirection, window_id: u32, focused_id: u32) -> bool {
        match &mut self.layout {
            Some(root_node) => {
                let focused_node = root_node.find_window_mut(focused_id);
                match focused_node {
                    Some(focused_node) => {
                        TilingNode::split_window(focused_node,direction,window_id);
                        true
                    },
                    None => false
                }
            },
            None => false
        }
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
    active_column: usize,
    columns: Vec<Column>
}

impl Monitor {
    pub fn new(id: u32, visible_columns: usize) -> Self {
        Monitor {
            id,
            visible_columns,
            active_column: 0,
            columns: Vec::new(),
        }
    }

    pub fn add_column(&mut self, column: Column) {
        self.columns.push(column)
    }

    pub fn rotate_columns(&mut self, motion: isize) { // Positive motion rotates right, negative
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

    pub fn move_active_column(&mut self, motion: isize) {
        let signed_active_column = self.active_column as isize;
        self.active_column = (signed_active_column + motion).rem_euclid(self.visible_columns as isize) as usize
    }
    pub fn scroll_active_column(&mut self, motion: isize) {
        let column = &mut self.columns[self.active_column];
        if !column.groups.is_empty() { column.scroll_column(motion)};
    }
    /// The group new windows land in: the active column's active group. Seeds one
    /// empty group in the active column if it has none yet (mirrors Group::add_window
    /// seeding its root leaf on first insert). Relies on the startup invariant that
    /// active_column always indexes a real column.
    pub fn active_group_mut(&mut self) -> &mut Group {
        let column = &mut self.columns[self.active_column];
        if column.groups.is_empty() {
            column.groups.push(Group { layout: None });
            column.active_row = 0;
        }
        &mut column.groups[column.active_row]
    }

    /// Evict a window from wherever it lives, searching every column's every group.
    /// Returns Removed on the first hit, NotFound if the id isn't present anywhere.
    /// TODO(max): does NOT prune a group that becomes empty -- empty-row pruning is
    /// a row-policy call that belongs to the scroll/row model.
    pub fn remove_window(&mut self, window_id: u32) -> RemoveResult {
        for column in &mut self.columns {
            for group in &mut column.groups {
                if matches!(group.remove_window(window_id), RemoveResult::Removed) {
                    return RemoveResult::Removed;
                }
            }
        }
        RemoveResult::NotFound
    }

    pub fn add_window(&mut self, direction: SplitDirection, id: u32, focused_id: u32) {
        for column in &mut self.columns {
            for group in &mut column.groups {
                if group.try_add_window(direction, id, focused_id) { return; }
            }
        }
        self.active_group_mut().add_window(direction,id,focused_id);
    }

    /// STUB -- the Monitor layout walk. Returns empty for now, so nothing tiles.
    /// TODO(max): Piece 3. Slice `bounds.width` into `visible_columns` equal bands
    /// left-to-right; for each visible column render its active group
    /// (`groups[active_row]`) via the free `compute_layout(tree, band_rect)`;
    /// concatenate the resulting Vec<(u32, Rect)> into one. An empty column, or a
    /// column whose active group has `layout: None`, reserves its band as blank
    /// space (do NOT collapse -- fixed-slot model). Band pixel width derives from
    /// `bounds` here; the stored Column.width field is unused for now.
    pub fn compute_layout(&self, bounds: Rect) -> Vec<(u32, Rect)> {
        let column_width = (bounds.width / self.visible_columns as u32) as usize;
        let columns = self.columns.iter().enumerate().take(self.visible_columns);
        let mut monitor_vec = Vec::new();
        for (i, c) in columns {
            match c.groups.get(c.active_row) {
                None => (),
                Some(group) => {
                    match &group.layout {
                        None => (),
                        Some(layout) => {
                            let column_bounds = Rect {
                                x: bounds.x + (i * column_width) as u32,
                                y: bounds.y,
                                width: column_width as u32,
                                height: bounds.height,
                            };
                            let column_vec = compute_layout(layout, column_bounds);
                            monitor_vec.extend(column_vec);
                        }
                    }
                }
            }
        }
        monitor_vec
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
