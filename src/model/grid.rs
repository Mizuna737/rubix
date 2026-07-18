use super::tiling::TilingNode;

pub struct Group {
    pub layout: TilingNode,
}

impl Group {
    pub fn count_windows(&self) -> usize {
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

    pub fn count_windows(&self) -> usize {
        self.groups.iter().map(|g| g.count_windows()).sum()
    }

    pub fn add_group(&mut self, group_id: Group) -> () {
        self.groups.push(group_id)
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

    pub fn count_windows(&self) -> usize {
        self.columns.iter().map(|c| c.count_windows()).sum()
    }

    pub fn add_column(&mut self, column_id: Column) -> () {
        self.columns.push(column_id)
    }
}
