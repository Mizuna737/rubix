use super::tiling::{TilingNode, SplitDirection};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
impl Rect {
    pub fn longer_axis(self) -> SplitDirection {
        if self.width >= self.height { SplitDirection::Horizontal } else { SplitDirection::Vertical }
    }
}
pub fn compute_layout(node: &TilingNode, bounds: Rect, inner_gap: u32) -> Vec<(u32, Rect)> {
    match node {
        TilingNode::Leaf { window_id } => {
            vec![(*window_id,bounds)]
        }
        TilingNode::Split { split_direction, split_ratio, left_child, right_child } => {
            let usable_bounds = Rect {
                x: bounds.x,
                y: bounds.y,
                width: bounds.width.saturating_sub(inner_gap),
                height: bounds.height.saturating_sub(inner_gap),
            };
            let (left_bounds, right_bounds) = match split_direction {
                SplitDirection::Horizontal => {
                    let left = Rect {
                        x: bounds.x,
                        y: bounds.y,
                        width: (usable_bounds.width as f32 * *split_ratio) as u32,
                        height: bounds.height,
                    };
                    let right = Rect {
                        x: bounds.x + left.width + inner_gap,
                        y: bounds.y,
                        width: usable_bounds.width - left.width,
                        height: bounds.height,
                    };
                    (left,right)
                },
                SplitDirection::Vertical => {
                    let left = Rect {
                        x: bounds.x,
                        y: bounds.y,
                        width: bounds.width,
                        height: (usable_bounds.height as f32 * *split_ratio) as u32,
                    };
                    let right = Rect {
                        x: bounds.x,
                        y: bounds.y + left.height + inner_gap,
                        width: bounds.width,
                        height: usable_bounds.height - left.height,
                    };
                    (left,right)
                }
            };
            let mut windows = compute_layout(left_child,left_bounds,inner_gap);
            windows.extend(compute_layout(right_child,right_bounds,inner_gap));
            windows
        }
    }
}

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;
