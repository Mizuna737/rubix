use super::tiling::{TilingNode, SplitDirection};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub fn compute_layout(node: &TilingNode, bounds: Rect) -> Vec<(u32, Rect)> {
    match node {
        TilingNode::Leaf { window_id } => {
            vec![(*window_id,bounds)]
        }
        TilingNode::Split { split_direction, split_ratio, left_child, right_child } => {
            let (left_bounds, right_bounds) = match split_direction {
                SplitDirection::Horizontal => {
                    let left = Rect {
                        x: bounds.x,
                        y: bounds.y,
                        width: (bounds.width as f32 * *split_ratio) as u32,
                        height: bounds.height,
                    };
                    let right = Rect {
                        x: bounds.x + left.width,
                        y: bounds.y,
                        width: bounds.width - left.width,
                        height: bounds.height,
                    };
                    (left,right)
                },
                SplitDirection::Vertical => {
                    let left = Rect {
                        x: bounds.x,
                        y: bounds.y,
                        width: bounds.width,
                        height: (bounds.height as f32 * *split_ratio) as u32,
                    };
                    let right = Rect {
                        x: bounds.x,
                        y: bounds.y + left.height,
                        width: bounds.width,
                        height: bounds.height - left.height,
                    };
                    (left,right)
                }
            };
            let mut windows = compute_layout(left_child,left_bounds);
            windows.extend(compute_layout(right_child,right_bounds));
            windows
        }
    }
}

#[cfg(test)]
#[path = "geometry_tests.rs"]
mod tests;
