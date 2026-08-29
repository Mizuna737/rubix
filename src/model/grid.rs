use super::tiling::{TilingNode, SplitDirection, RemoveResult};
use super::traits::{CountWindows, sum_windows};
use super::geometry::{compute_layout, Rect};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RevealKind {
    AlreadyVisible,
    Scrolled { down: bool },
    /// The cube was spun so an off-screen column reached the visible band.
    /// Every column's active group travels, so this is a whole-screen motion --
    /// it maps to Transition::Rotate, not to a single group appearing in place.
    Rotated,
    /// The target was too far away to spin to, so its group traded places with
    /// the one in the active slot. Nothing slides: the two columns are not
    /// adjacent, so there is no edge to travel toward that would read as motion
    /// rather than a glitch. Maps to Transition::Reveal.
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

    /// Insert a fresh column, seeded with one empty group, at `index`.
    pub fn grow_columns(&mut self, index: usize) -> &mut Column {
        let mut column = Column::new(0);
        column.add_group(Group::empty());
        let index = index.min(self.columns.len());
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

    /// Point the cursor at `(col, row)` without naming a window in it. The slot
    /// counterpart to `focus_window`, for destinations that hold none -- an
    /// empty group is a legitimate place for focus to land so that a subsequent
    /// spawn has a home. Leaves the group's remembered `active_window` alone.
    pub fn set_active_slot(&mut self, col: usize, row: usize) -> bool {
        let Some(column) = self.columns.get_mut(col) else { return false };
        if row >= column.groups.len() {
            return false;
        }
        column.active_row = row;
        self.active_column = col;
        true
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
        // No direction: an activation request or a click carries no sense of
        // travel, so reveal_slot picks the cheaper rotation (or swaps).
        self.reveal_slot(col, row, None).map(|(kind, _, _)| kind)
    }

    /// Bring the group at `(col, row)` on screen, and report where it ended up.
    ///
    /// The returned slot is NOT always the one asked for: the rotation branch
    /// moves the group to a different column index entirely. Callers holding a
    /// window id can re-`locate` it afterwards, but a caller revealing an EMPTY
    /// group has no id to look up -- hence the coordinates come back here.
    ///
    /// Focus-neutral in every branch: this touches active_row and group
    /// placement, never `active_column`. Pair it with `focus_window` or
    /// `set_active_slot` when the cursor should follow.
    pub fn reveal_slot(
        &mut self,
        col: usize,
        row: usize,
        travel: Option<Direction>,
    ) -> Option<(RevealKind, usize, usize)> {
        if row >= self.columns.get(col)?.groups.len() {
            return None;
        }

        if col < self.visible_columns {
            let column = &mut self.columns[col];
            if column.active_row == row {
                return Some((RevealKind::AlreadyVisible, col, row));
            }
            let down = row > column.active_row;
            column.active_row = row;
            return Some((RevealKind::Scrolled { down }, col, row));
        }

        // Off screen. Scroll the target column to the target row FIRST:
        // rotate_columns only carries each column's active group, so a group
        // sitting on any other row would hold still while the cube turned around
        // it and never arrive.
        self.columns[col].active_row = row;

        // Rotation is cyclic, so an off-screen column can be reached from either
        // side, and the cheap one depends on which way the user was travelling.
        // POSITIVE motion moves every active group one column RIGHT (the last
        // wrapping around to the front) -- rotate_columns swaps repeatedly
        // through slot 0, so the sign is the opposite of what the loop shape
        // suggests.
        //
        //   travelling Right -> land at the right edge of the visible band, so
        //                       the target slides in from the side it was hiding
        //                       behind. Rotate LEFT.
        //   travelling Left  -> the target is the LAST column (a wrap), so bring
        //                       it around into slot 0. Rotate RIGHT.
        //
        // Both are exactly one step for any single nav press, which is what
        // keeps ordinary navigation out of the swap branch below.
        let right_edge = self.visible_columns.saturating_sub(1).min(col);
        let left_steps = col - right_edge;
        let right_steps = self.columns.len() - col;
        let rotate_right = match travel {
            Some(Direction::Right) => false,
            Some(Direction::Left) => true,
            // No direction to follow (foreign-toplevel activation, click to
            // focus): take whichever way is shorter.
            _ => right_steps < left_steps,
        };
        let steps = if rotate_right { right_steps } else { left_steps };

        // Too far to spin: a multi-column rotation drags every other column
        // across the screen for what the user experienced as a jump to one
        // window. Trade the target into the active slot instead -- only reachable
        // from foreign-toplevel activation and click-to-focus, since every nav
        // press resolves to a single step.
        if steps > 1 {
            return self.swap_slot_into_view(col, row);
        }

        self.rotate_columns(if rotate_right { steps as isize } else { -(steps as isize) });
        let dest_col = if rotate_right { 0 } else { right_edge };
        // Rotation swaps group CONTENTS between each column's active row and
        // leaves every active_row index untouched, so the target now sits at the
        // destination column's own active row -- not at `row`.
        let dest_row = self.columns[dest_col].active_row;
        Some((RevealKind::Rotated, dest_col, dest_row))
    }

    /// Trade the group at `(col, row)` with whatever occupies the active slot.
    /// The far-jump path: see `reveal_slot`'s `steps > 1` branch.
    fn swap_slot_into_view(&mut self, col: usize, row: usize) -> Option<(RevealKind, usize, usize)> {
        // The active column is the natural landing site, but it is NOT
        // guaranteed to be on screen. DecrementVisibleColumns shrinks the
        // visible range without clamping active_column, and focus_window sets
        // active_column with no bound at all -- so the cursor can sit past
        // visible_columns. Clamping into the visible range is what makes the
        // destination a slot compute_layout actually emits, and it is also what
        // keeps the swap sound: callers reach here only with col >=
        // visible_columns, so dest_col is strictly less than col and
        // split_at_mut(col) is guaranteed to put the two in different halves.
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
        Some((RevealKind::Swapped, dest_col, dest_row))
    }

    pub fn find_group_by_direction(&self, window_id: u32, direction: Direction) -> Option<(usize,usize)> {
        let (c, g) = self.find_column_and_row_by_window_id(window_id)?;
        // Relocating a window may target any column, on screen or not.
        self.neighbour_slot(c, g, direction, self.columns.len())
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
    /// Promote `window_id` into a brand-new column beside the active one --
    /// before it when `to_left`, after it otherwise -- and leave the cursor on
    /// the new column.
    ///
    /// Widens the visible band by one on the way out. Inserting a column
    /// otherwise pushes whatever sat at the right edge off screen, so the
    /// promoted window would land somewhere the renderer never draws.
    pub fn move_window_to_new_column(&mut self, window_id: u32, to_left: bool) {
        if !self.detach_window(window_id) {
            return;
        }
        let index = if to_left { self.active_column } else { self.active_column + 1 };
        let new_column = self.grow_columns(index);
        new_column.groups[0].add_window(SplitDirection::Horizontal, window_id, 0);
        // Set the cursor BEFORE widening: increment_visible_columns clamps
        // active_column into the new band, which is exactly the guard wanted
        // here rather than something to work around.
        self.active_column = index.min(self.columns.len().saturating_sub(1));
        self.increment_visible_columns(1);
    }

    /// The slot next to `(c, g)` in `direction`, or None at the edge.
    ///
    /// Vertical steps stay inside the column; horizontal ones land on the
    /// destination column's ACTIVE row -- the group actually on screen beside
    /// you, not the one that happens to share your row index. `column_bound`
    /// caps horizontal travel: `columns.len()` to reach the whole cube,
    /// `visible_columns` to stay on screen.
    fn neighbour_slot(
        &self,
        c: usize,
        g: usize,
        direction: Direction,
        column_bound: usize,
    ) -> Option<(usize, usize)> {
        match direction {
            Direction::Up if g > 0 => Some((c, g - 1)),
            Direction::Down if g + 1 < self.columns.get(c)?.groups.len() => Some((c, g + 1)),
            Direction::Left if c > 0 => Some((c - 1, self.columns[c - 1].active_row)),
            Direction::Right if c + 1 < column_bound.min(self.columns.len()) => {
                Some((c + 1, self.columns[c + 1].active_row))
            }
            _ => None,
        }
    }

    /// Trade the active group with its neighbour in `direction`, carrying the
    /// cursor along so the group you moved stays under you.
    ///
    /// Does NOT wrap, unlike directional focus: swapping at an edge would fling
    /// a group across the whole cube, which is not what "trade with the one
    /// beside me" means. Horizontal swaps are additionally capped to the visible
    /// band -- sending your own group off screen and following it there is never
    /// the intent.
    pub fn swap_active_group(&mut self, direction: Direction) -> bool {
        let c = self.active_column;
        let Some(column) = self.columns.get(c) else { return false };
        let g = column.active_row;
        let bound = self.visible_columns.max(1);
        let Some((dest_c, dest_g)) = self.neighbour_slot(c, g, direction, bound) else {
            return false;
        };

        if c == dest_c {
            self.columns[c].groups.swap(g, dest_g);
        } else {
            // split_at_mut(hi) puts the lower column in `left` and the higher at
            // right[0]; the rows have to follow the same ordering or the swap
            // reads the wrong group out of one of them.
            let (lo, hi) = if c < dest_c { (c, dest_c) } else { (dest_c, c) };
            let (lo_row, hi_row) = if c < dest_c { (g, dest_g) } else { (dest_g, g) };
            let (left, right) = self.columns.split_at_mut(hi);
            std::mem::swap(&mut left[lo].groups[lo_row], &mut right[0].groups[hi_row]);
        }

        self.active_column = dest_c;
        self.columns[dest_c].active_row = dest_g;
        true
    }
    /// Guarantee every column holds at least one group.
    ///
    /// `rotate_columns` indexes `groups[active_row]` on EVERY column, so one
    /// with no groups at all is an index panic. That state is reachable --
    /// `add_column` takes a bare `Column` and nothing prunes -- and seeding an
    /// empty group restores the invariant `Monitor::new` and `grow_columns`
    /// establish rather than skipping the column and breaking the rotation.
    fn seed_empty_columns(&mut self) {
        for column in &mut self.columns {
            if column.groups.is_empty() {
                column.groups.push(Group::empty());
                column.active_row = 0;
            }
        }
    }

    pub fn rotate_columns(&mut self, motion: isize) {
        self.seed_empty_columns();
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

    /// Prune the empty group the cursor is parked on, and the column with it
    /// when that was the column's last group.
    ///
    /// Only ever removes a group with no layout at all -- a group holding
    /// windows is not something a keypress should be able to delete out from
    /// under them. Returns whether anything was removed.
    pub fn remove_active_group(&mut self) -> bool {
        let c = self.active_column;
        let Some(column) = self.columns.get(c) else { return false };
        let g = column.active_row;
        if column.groups.get(g).map_or(true, |group| group.layout.is_some()) {
            return false;
        }
        // One empty group in one column is the resting state of an empty
        // monitor, not something to prune: removing it would leave the cursor
        // pointing at nothing and every active_* accessor indexing an empty Vec.
        if self.columns.len() == 1 && column.groups.len() == 1 {
            return false;
        }

        let column = &mut self.columns[c];
        column.groups.remove(g);
        if column.groups.is_empty() {
            self.columns.remove(c);
        } else if column.active_row >= column.groups.len() {
            column.active_row = column.groups.len() - 1;
        }
        // Dropping a column can leave the visible band wider than the cube and
        // the cursor past its end. increment_visible_columns(0) re-clamps both,
        // which is the whole reason its clamps are unconditional.
        self.increment_visible_columns(0);
        true
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
    /// The id of the monitor `motion` steps from the active one, wrapping.
    /// None when there is nowhere else to go.
    ///
    /// Order is `monitors` list order -- output discovery order -- NOT spatial
    /// left-to-right. The model holds no output geometry to sort by; that lives
    /// in the compositor's output map. Worth knowing before this feels wrong on
    /// a head plugged in out of order.
    pub fn monitor_id_by_offset(&self, motion: isize) -> Option<u32> {
        if self.monitors.len() < 2 {
            return None;
        }
        let index = self
            .monitors
            .iter()
            .position(|m| m.id == self.active_monitor)
            .unwrap_or(0) as isize;
        let next = (index + motion).rem_euclid(self.monitors.len() as isize) as usize;
        Some(self.monitors[next].id)
    }

    /// Detach `window_id` from whichever monitor holds it and drop it into
    /// `dest_id`'s active group, moving the cursor to that monitor with it.
    ///
    /// `split` decides how the destination's active group divides to make room;
    /// the caller computes it, since the rule depends on output geometry the
    /// model cannot see.
    pub fn move_window_to_monitor(&mut self, window_id: u32, dest_id: u32, split: SplitDirection) -> bool {
        if self.get_monitor_id_by_window_id(window_id) == Some(dest_id) {
            return false;
        }
        let Some(dest_index) = self.monitors.iter().position(|m| m.id == dest_id) else {
            return false;
        };
        if !self.monitors.iter_mut().any(|m| m.remove_window(window_id)) {
            return false;
        }
        let dest = &mut self.monitors[dest_index];
        // Land beside whatever is focused over there, the way a freshly mapped
        // window would. active_window() is None for an empty group, and 0 is not
        // a live id, so add_window falls through to seeding the group.
        let anchor = dest.active_window().unwrap_or(0);
        dest.active_group_mut().add_window(split, window_id, anchor);
        self.active_monitor = dest_id;
        true
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
