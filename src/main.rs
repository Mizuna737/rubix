mod model;
use model::tiling::{TilingNode, SplitDirection};
fn main() {
    let window_1 = TilingNode::Leaf {window_id: 1 };
    let window_2 = TilingNode::Leaf {window_id: 2 };
    let window_3 = TilingNode::Leaf {window_id: 3 };
    let window_4 = TilingNode::Leaf {window_id: 4 };

    let split_1 = TilingNode::Split {
        split_direction: SplitDirection::Horizontal,
        split_ratio: 0.3,
        left_child: Box::new(window_1),
        right_child: Box::new(window_2)
    };
    let split_2 = TilingNode::Split {
        split_direction: SplitDirection::Vertical,
        split_ratio: 0.2,
        left_child: Box::new(window_3),
        right_child: Box::new(window_4)
    };
    
    let mut layout = TilingNode::Split {
        split_direction: SplitDirection::Vertical,
        split_ratio: 0.5,
        left_child: Box::new(split_1),
        right_child: Box::new(split_2)
    };

    println!("windows: {}", layout.count_windows());

    println!("{layout:?}");
    layout.change_split_ratio(0.5);
    println!("{layout:?}");
}
