use super::tiling::{TilingNode, RemoveResult};
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
        match &mut self.layout {
            Some(node) => node.remove_window(window_id),
            None => RemoveResult:: NotFound
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

    fn rotate_columns(&mut self, motion: isize) { /// Positive motion rotates right, negative
                                                  /// rotates left.
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
}
