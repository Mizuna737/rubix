use super::tiling::{TilingNode, SplitDirection, RemoveResult};
use super::traits::{CountWindows, sum_windows};
use super::geometry::{compute_layout, Rect};

pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}
pub enum RevealKind {
    AlreadyVisible,
    Scrolled { down: bool },
    Swapped,
}
pub struct Group {
    pub layout: Option<TilingNode>,
    pub active_window: Option<u32>,
}

impl Group {
    pub fn new( layout: TilingNode, window_id: u32 ) -> Self {
        Group {
            layout: Some(layout),
            active_window: Some(window_id),
        }
    }
    pub fn empty() -> Self {
        Group {
            layout: None, active_window: None
        }
    }
    pub fn remove_window(&mut self, window_id: u32) -> bool {
        let result = match &mut self.layout {
            Some(node) => node.remove_window(window_id),
            None => return false,
        };

        match result {
            RemoveResult::Removed { survivor_id } => {
                if self.active_window == Some(window_id) {
                    self.active_window = Some(survivor_id);
                }
                true
            }
            RemoveResult::NotFound => false,
            RemoveResult::RemoveMe => {
                self.layout = None;
                self.active_window = None;
                true
            }
        }
    }
    /// Window ids in this group's tree, in tree order. Read-only view for the
    /// IPC snapshot (see ipc.rs) -- serde stays out of this module entirely.
    pub fn window_ids(&self) -> Vec<u32> {
        match &self.layout {
            Some(node) => node.collect_ids(),
            None => Vec::new(),
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
        self.active_window = Some(window_id);
    }
    pub fn try_add_window(&mut self, direction: SplitDirection, window_id: u32, focused_id: u32) -> bool {
        match &mut self.layout {
            Some(root_node) => {
                let focused_node = root_node.find_window_mut(focused_id);
                match focused_node {
                    Some(focused_node) => {
                        TilingNode::split_window(focused_node,direction,window_id);
                        self.active_window = Some(window_id);
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
    /// Reserved for manual column-band widths (roadmap Tier 1). compute_layout still
    /// divides the monitor into equal bands and ignores this, so it is written by
    /// `new` and never read. Kept as the hook that feature will use.
    #[allow(dead_code)]
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

    pub fn active_row(&self) -> usize {
        self.active_row
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }
}

impl CountWindows for Column {
    fn count_windows(&self) -> usize {
        sum_windows(&self.groups)
    }
}
pub struct Monitor {
    pub id: u32,
    visible_columns: usize,
    active_column: usize,
    columns: Vec<Column>
}

impl Monitor {
    pub fn new(id: u32, visible_columns: usize) -> Self {
        let mut monitor = Monitor {
            id,
            visible_columns,
            active_column: 0,
            columns: Vec::new(),
        };
        for _ in 0..visible_columns {
            let mut column = Column::new(0);
            column.add_group(Group::empty());
            monitor.columns.push(column);
        }
        monitor
    }

    pub fn increment_visible_columns(&mut self, change: isize) {
        let new = self.visible_columns as isize + change;
        self.visible_columns = new.clamp(1,self.columns.len().max(1) as isize) as usize;
        // compute_layout emits columns[0..visible_columns), so the cursor has to stay
        // inside that band. Shrinking used to leave active_column pointing at a column
        // that had just gone off screen, and nav (scroll_active_column, grow_active_column,
        // active_window) kept operating on it until the cursor happened to move back.
        // The clamp above guarantees visible_columns >= 1, so the subtraction is sound.
        self.active_column = self.active_column.min(self.visible_columns - 1);
    }

    pub fn active_window(&self) -> Option<u32> {
        let column = self.columns.get(self.active_column)?;
        let group = column.groups.get(column.active_row)?;
        let node = group.layout.as_ref()?;   // empty group: no active window, done

        Some(group.active_window
            .filter(|id| node.find_window(*id).is_some())
            .unwrap_or_else(|| node.find_first_leaf_id()))
    }

    pub fn add_column(&mut self, column: Column) {
        self.columns.push(column)
    }

    pub fn active_column(&self) -> usize {
        self.active_column
    }

    pub fn visible_columns(&self) -> usize {
        self.visible_columns
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn grow_columns(&mut self) -> &mut Column {
        let mut column = Column::new(0);
        column.add_group(Group::empty());
        let index = self.active_column + 1;
        self.columns.insert(index, column);
        &mut self.columns[index]
    }

    fn detach_window(&mut self, window_id: u32) -> bool {
        for column in &mut self.columns {
            for group in &mut column.groups {
                if matches!(group.remove_window(window_id), true) {
                    return true;
                }
            }
        }
        false
    }

    fn find_column_and_row_by_window_id(&self, window_id: u32) -> Option<(usize,usize)> {
        for c in 0 .. self.columns.len() {
            for g in 0 .. self.columns[c].groups.len() {
                if self.columns[c].groups[g].layout.as_ref()
                    .is_some_and(|node| node.find_window(window_id).is_some())
                {
                    return Some((c, g));
                }
            }
        }
        None
    }

    pub fn locate(&self, window_id: u32) -> Option<(usize,usize)> {
        self.find_column_and_row_by_window_id(window_id)
    }

    pub fn focus_window(&mut self, window_id: u32) -> bool {
        let Some((col,row)) = self.locate(window_id) else { return false };
        self.active_column = col;
        self.columns[col].active_row = row;
        self.columns[col].groups[row].active_window = Some(window_id);
        true
    }



    pub fn reveal_window(&mut self, window_id: u32) -> Option<RevealKind> {
        let (col, row) = self.locate(window_id)?;
        if col < self.visible_columns {
            let column = &mut self.columns[col];
            if column.active_row == row {
                return Some(RevealKind::AlreadyVisible);
            }
            let down = row > column.active_row;
            column.active_row = row;
            return Some(RevealKind::Scrolled { down });
        }

        // The active column is the natural landing site, but it is NOT
        // guaranteed to be on screen. DecrementVisibleColumns shrinks the
        // visible range without clamping active_column, and focus_window sets
        // active_column with no bound at all -- so the cursor can sit past
        // visible_columns. Clamping into the visible range is what makes the
        // destination a slot compute_layout actually emits, and it is also what
        // keeps the swap sound: this branch has col >= visible_columns, so
        // dest_col is strictly less than col and split_at_mut(col) is
        // guaranteed to put the two in different halves.
        let dest_col = self.active_column.min(self.visible_columns.saturating_sub(1));
        debug_assert!(dest_col < col, "reveal destination must precede the split point");

        // Seed an empty destination rather than indexing an empty Vec. The
        // destination is not necessarily the active column, so seeding via
        // active_group_mut would guard the wrong one.
        if self.columns[dest_col].groups.is_empty() {
            self.columns[dest_col].groups.push(Group::empty());
            self.columns[dest_col].active_row = 0;
        }
        let dest_row = self.columns[dest_col].active_row;

        let (left, right) = self.columns.split_at_mut(col);
        std::mem::swap(
            &mut left[dest_col].groups[dest_row],
            &mut right[0].groups[row],
            );
        Some(RevealKind::Swapped)
    }

    pub fn find_group_by_direction(&self, window_id: u32, direction: Direction) -> Option<(usize,usize)> {
        let Some((c,g)) = self.find_column_and_row_by_window_id(window_id) else {
            return None;
        };
        match direction {
            Direction::Up if g > 0 => Some((c, g - 1)),
            Direction::Down if g < self.columns[c].groups.len() - 1 => Some((c, g + 1)),
            Direction::Left if c > 0 => Some((c - 1, self.columns[c - 1].active_row)),
            Direction::Right if c < self.columns.len() - 1 => Some((c + 1, self.columns[c + 1].active_row)),
            _ => None,
        }
    }
    pub fn find_first_leaf_id(&self, target_c: usize, target_g: usize) -> Option<u32> {
        let column = self.columns.get(target_c)?;
        let group = column.groups.get(target_g)?;
        group.layout.as_ref().map(|node| node.find_first_leaf_id())
    }
    pub fn move_window_to_group(&mut self, window_id: u32, target_c: usize, target_g: usize, split_direction: SplitDirection) {
        if !self.detach_window(window_id) {
            return;
        }
        self.active_column = target_c;
        self.columns[target_c].active_row = target_g;
        self.columns[target_c].groups[target_g]
            .add_window(split_direction, window_id, 0);
    }
    pub fn flip_split_direction(&mut self, id: u32) {
        for column in &mut self.columns {
            for group in &mut column.groups {
                if let Some(node) = group.layout.as_mut() {
                    if node.flip_parent_split_direction(id) {
                        return;
                    }
                }
            }
        }
    }
    pub fn move_window_to_new_column(&mut self, window_id: u32) {
        if !self.detach_window(window_id) {
            return;
        }
        let new_column = self.grow_columns();
        new_column.groups[0].add_window(SplitDirection::Horizontal, window_id, 0);
    }
    pub fn rotate_columns(&mut self, motion: isize) {
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
        column.groups.insert(column.active_row + 1, Group::empty());
        column.scroll_column(1);
    }
    /// The group new windows land in: the active column's active group. Seeds one
    /// empty group in the active column if it has none yet (mirrors Group::add_window
    /// seeding its root leaf on first insert). Relies on the startup invariant that
    /// active_column always indexes a real column.
    pub fn active_group_mut(&mut self) -> &mut Group {
        let column = &mut self.columns[self.active_column];
        if column.groups.is_empty() {
            column.groups.push(Group::empty());
            column.active_row = 0;
        }
        &mut column.groups[column.active_row]
    }

    /// Evict a window from wherever it lives, searching every column's every group.
    /// Returns Removed on the first hit, NotFound if the id isn't present anywhere.
    /// TODO(max): does NOT prune a group that becomes empty -- empty-row pruning is
    /// a row-policy call that belongs to the scroll/row model.
    pub fn remove_window(&mut self, window_id: u32) -> bool {
        for column in &mut self.columns {
            for group in &mut column.groups {
                if matches!(group.remove_window(window_id), true) {
                    return true;
                }
            }
        }
        false
    }

    pub fn add_window(&mut self, direction: SplitDirection, id: u32, focused_id: u32) {
        for column in &mut self.columns {
            for group in &mut column.groups {
                if group.try_add_window(direction, id, focused_id) { return; }
            }
        }
        self.active_group_mut().add_window(direction,id,focused_id);
    }

    pub fn compute_layout(&self, bounds: Rect, outer_gap: u32, inner_gap: u32) -> Vec<(u32, Rect)> {
        let columns = self.columns.iter().enumerate().take(self.visible_columns);
        let mut monitor_vec = Vec::new();
        for (i, c) in columns {
            match c.groups.get(c.active_row) {
                None => (),
                Some(group) => {
                    match &group.layout {
                        None => (),
                        Some(layout) => {
                            // For gaps support we need to distinguish between edge columns and
                            // central columns. We also need to account for the edge case of only
                            // one visible column. Columns on edges offset bounds.x by outer_gap and
                            // subtract outer_gap from their width (for each edge). This is a
                            // surprisingly involved puzzle.
                            let half_gap_low = outer_gap / 2;
                            let half_gap_high = outer_gap - half_gap_low;
                            let usize_width = bounds.width as usize;
                            let x_left = bounds.x + (i * usize_width / self.visible_columns) as u32;
                            let x_right = bounds.x + ((i+1) * usize_width / self.visible_columns) as u32;
                            let left_inset = if i == 0 { outer_gap } else { half_gap_high };
                            let right_inset = if i == self.visible_columns - 1 { outer_gap } else { half_gap_low };
                            let column_bounds = Rect {
                                x: x_left + left_inset,
                                y: bounds.y + outer_gap,
                                width: (x_right - x_left).saturating_sub(left_inset + right_inset),
                                height: bounds.height.saturating_sub(2 * outer_gap),
                            };
                            let column_vec = compute_layout(layout, column_bounds, inner_gap);
                            monitor_vec.extend(column_vec);
                        }
                    }
                }
            }
        }
        monitor_vec
    }
}

pub struct Workspace {
    pub(crate) monitors: Vec<Monitor>,
    active_monitor: u32,
}

impl Workspace {
    pub fn new() -> Workspace {
        Workspace { monitors: Vec::new(), active_monitor: 0 }
    }
    pub fn active_monitor_mut(&mut self) -> Option<&mut Monitor> {
        if self.monitors.len() == 0 {
            return None;
        }
        for monitor in self.monitors.iter_mut() {
            if self.active_monitor == monitor.id {
                return Some(monitor);
            }
        }
        None
    }
    pub fn active_monitor(&self) -> Option<&Monitor> {
        if self.monitors.len() == 0 {
            return None;
        }
        for monitor in self.monitors.iter() {
            if self.active_monitor == monitor.id {
                return Some(monitor);
            }
        }
        None
    }
    /// Which monitor id nav and layout treat as current.
    ///
    /// Note this is an id, not an index -- it is matched against `Monitor::id`,
    /// and there is no guarantee a monitor with this id exists (an unplugged
    /// output leaves the id dangling until something sets it again), which is
    /// why `active_monitor()` returns an Option.
    pub fn active_monitor_id(&self) -> u32 {
        self.active_monitor
    }

    pub fn set_active_monitor(&mut self, id: u32) {
        self.active_monitor = id;
    }
    pub fn ensure_monitor(&mut self, id: u32, visible_columns: usize) -> &Monitor {
        if let Some(idx) = self.monitors.iter().position(|m| m.id == id) {
            &mut self.monitors[idx]
        } else {
            self.monitors.push(Monitor::new(id, visible_columns));
            self.monitors.last_mut().unwrap()
        }
    }
    pub fn get_monitor_id_by_window_id(&self, window_id: u32) -> Option<u32> {
        for monitor in self.monitors.iter() {
            for column in monitor.columns.iter() {
                for group in column.groups.iter() {
                    if group.layout.as_ref().and_then(|n| n.find_window(window_id)).is_some() {
                        return Some(monitor.id);
                    }
                }
            }
        }
        return None;
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
