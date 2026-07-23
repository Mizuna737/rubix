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
                    None => {
                        let first_leaf = root_node.find_first_leaf_mut();
                        TilingNode::split_window(first_leaf,direction,window_id)
                    },
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

    pub fn active_window(&self) -> Option<u32> {
        let column = self.columns.get(self.active_column)?;
        let group = column.groups.get(column.active_row)?;

        group.layout.as_ref().map(|node| node.find_first_leaf_id())
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
    pub fn grow_active_column(&mut self) {
        let column = &mut self.columns[self.active_column];
        column.groups.insert(column.active_row + 1, Group { layout: None });
        column.scroll_column(1);
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
#[path = "grid_tests.rs"]
mod tests;
