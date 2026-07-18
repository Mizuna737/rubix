#[derive(Debug)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug)]
pub enum TilingNode {
Split   { split_direction: SplitDirection, split_ratio: f32, left_child: Box<TilingNode>, right_child: Box<TilingNode> },
Leaf    { window_id: u32 },
}

impl TilingNode {
    pub fn count_windows(&self) -> usize {
        match self {
            TilingNode::Leaf { .. } => 1,
            TilingNode::Split { left_child, right_child, .. } => {
                left_child.count_windows() + right_child.count_windows()
            }
        }
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
}
