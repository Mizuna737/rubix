use super::tiling::{TilingNode};
use super::traits::{CountWindows, sum_windows};
pub struct Group {
    pub layout: TilingNode,
}

impl CountWindows for Group {
    fn count_windows(&self) -> usize {
        self.layout.count_windows()
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
}

impl CountWindows for Column {
    fn count_windows(&self) -> usize {
        sum_windows(&self.groups)
    }
}
pub struct Monitor {
    id: u32,
    visible_columns: usize,
    viewport_offset: usize,
    columns: Vec<Column>
}

impl Monitor {
    pub fn new(id: u32, visible_columns: usize, viewport_offset: usize) -> Self {
        Monitor {
            id,
            visible_columns,
            viewport_offset,
            columns: Vec::new(),
        }
    }

    pub fn add_column(&mut self, column: Column) {
        self.columns.push(column)
    }
}

impl CountWindows for Monitor {
    fn count_windows(&self) -> usize {
        sum_windows(&self.columns)
    }
}
