use super::traits::{CountWindows};

#[derive(Clone,Copy)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

impl SplitDirection {
    pub fn toggled(self) -> Self {
        match self {
            SplitDirection::Horizontal => SplitDirection::Vertical,
            SplitDirection::Vertical => SplitDirection::Horizontal
        }
    }
}
pub enum TilingNode {
Split   { split_direction: SplitDirection, split_ratio: f32, left_child: Box<TilingNode>, right_child: Box<TilingNode> },
Leaf    { window_id: u32 },
}
pub enum RemoveResult {
    RemoveMe,
    Removed,
    NotFound,
}
impl TilingNode {
    pub fn new(id: u32) -> Self {
        TilingNode::Leaf { window_id: id }
    }
    pub fn change_split_ratio(&mut self, value: f32) {
        match self {
            TilingNode::Leaf { .. } => (),
            TilingNode::Split { left_child, right_child, split_ratio, .. } => {
                
                left_child.change_split_ratio(value);
                right_child.change_split_ratio(value);

                *split_ratio = value;
            }
        }
    }

    fn is_leaf(&self, id: u32) -> bool {
        if matches!(self, TilingNode::Leaf { window_id } if *window_id == id ) { true } else { false }
    }
    pub fn flip_parent_split_direction(&mut self, target_id: u32) -> bool {
        match self {
            TilingNode::Leaf { window_id }=> {
                if *window_id == target_id {
                    true
                }
                else {
                    false
                }
            }
            TilingNode::Split { split_direction, left_child, right_child, .. } => {
                if left_child.is_leaf(target_id) || right_child.is_leaf(target_id) {
                    *split_direction = split_direction.toggled();
                    true
                }
                else {
                    left_child.flip_parent_split_direction(target_id) || right_child.flip_parent_split_direction(target_id)
                }
            }
        }
    }
    pub fn find_window(&self, target_id: u32) -> Option<&TilingNode> {
        match self {
            TilingNode::Leaf { window_id } => {
                if *window_id == target_id {
                    Some(&self)
                }
                else {
                    None
                }
            }
            TilingNode::Split { left_child, right_child, ..} => {
                let left_search = left_child.find_window(target_id);
                if left_search.is_some() {
                    left_search
                }
                else {
                    right_child.find_window(target_id)
                }
            }
        }
    }
    pub fn find_window_mut(&mut self, target_id: u32) -> Option<&mut TilingNode> {
        match self {
            TilingNode::Leaf { window_id } => {
                if *window_id == target_id {
                    Some(self)
                }
                else {
                    None
                }
            }
            TilingNode::Split { left_child, right_child, ..} => {
                let left_search = left_child.find_window_mut(target_id);
                if left_search.is_some() {
                    left_search
                }
                else {
                    right_child.find_window_mut(target_id)
                }
            }
        }
    }
    pub fn split_window(node: &mut TilingNode, direction: SplitDirection, new_window_id: u32 ) {
        match node {
            TilingNode::Split { .. } => {
                panic!("split_window called on a split!");
            }
            TilingNode::Leaf { window_id } => {
                let id = *window_id;
                *node = TilingNode::Split {
                    split_direction: direction,
                    split_ratio: 0.5,
                    left_child: Box::new(TilingNode::Leaf { window_id: id }),
                    right_child: Box::new(TilingNode::Leaf { window_id: new_window_id }),
                };
            }
        }
    }
    pub fn remove_window(&mut self, target_id: u32) -> RemoveResult {
        match self {
            TilingNode::Leaf { window_id } => {
                if *window_id == target_id {
                    RemoveResult::RemoveMe
                }
                else {
                    RemoveResult::NotFound
                }
            }
            TilingNode::Split { left_child, right_child, .. } => {
                match left_child.remove_window(target_id) {
                    RemoveResult::Removed => RemoveResult::Removed,
                    RemoveResult::RemoveMe => {
                        let surviving = std::mem::replace(right_child, Box::new(TilingNode::Leaf {window_id: 0}));
                        *self = *surviving;
                        RemoveResult::Removed
                    },
                    RemoveResult::NotFound => match right_child.remove_window(target_id) {
                        RemoveResult::Removed => RemoveResult::Removed,
                        RemoveResult::RemoveMe => {
                            let surviving = std::mem::replace(left_child, Box::new(TilingNode::Leaf {window_id: 0}));
                            *self = *surviving;
                            RemoveResult::Removed
                        },
                        RemoveResult::NotFound => RemoveResult::NotFound
                    }
                }
            }
        }
    }
    pub fn find_first_leaf_mut(&mut self) -> &mut TilingNode {
        match self {
            TilingNode::Leaf { .. } => self,
            TilingNode::Split { left_child, .. } => {
                left_child.find_first_leaf_mut()
            }
        }
    }

    pub fn find_first_leaf_id(& self) -> u32 {
        match self {
            TilingNode::Leaf { window_id } => *window_id,
            TilingNode::Split { left_child, .. } => {
                left_child.find_first_leaf_id()
            }
        }
    }

    /// All leaf window ids under this node, left-to-right tree order. Plain
    /// data walk for the IPC snapshot (see model/grid.rs::Group::window_ids) --
    /// no serde here.
    pub fn collect_ids(&self) -> Vec<u32> {
        let mut ids = Vec::new();
        self.collect_ids_into(&mut ids);
        ids
    }

    fn collect_ids_into(&self, out: &mut Vec<u32>) {
        match self {
            TilingNode::Leaf { window_id } => out.push(*window_id),
            TilingNode::Split { left_child, right_child, .. } => {
                left_child.collect_ids_into(out);
                right_child.collect_ids_into(out);
            }
        }
    }
}

impl CountWindows for TilingNode {
    fn count_windows(&self) -> usize {
        match self {
            TilingNode::Leaf { .. } => 1,
            TilingNode::Split { left_child, right_child, .. } => {
                left_child.count_windows() + right_child.count_windows()
            }
        }
    }
}

#[cfg(test)]
#[path = "tiling_tests.rs"]
mod tests;
