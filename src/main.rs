mod model;
use model::tiling::{TilingNode, SplitDirection};
use model::grid::{Group, Column, Monitor};
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
    
    let split_3 = TilingNode::Split {
        split_direction: SplitDirection::Vertical,
        split_ratio: 0.5,
        left_child: Box::new(split_1),
        right_child: Box::new(split_2)
    };

    let group_1 = Group {
        layout: split_3
    };
    let window_5 = TilingNode::Leaf {window_id: 1 };
    let window_6 = TilingNode::Leaf {window_id: 2 };
    let window_7 = TilingNode::Leaf {window_id: 3 };
    let window_8 = TilingNode::Leaf {window_id: 4 };

    let split_4 = TilingNode::Split {
        split_direction: SplitDirection::Horizontal,
        split_ratio: 0.3,
        left_child: Box::new(window_5),
        right_child: Box::new(window_6)
    };
    let split_5 = TilingNode::Split {
        split_direction: SplitDirection::Vertical,
        split_ratio: 0.2,
        left_child: Box::new(window_7),
        right_child: Box::new(window_8)
    };
    
    let split_6 = TilingNode::Split {
        split_direction: SplitDirection::Vertical,
        split_ratio: 0.5,
        left_child: Box::new(split_4),
        right_child: Box::new(split_5)
    };

    let group_2 = Group {
        layout: split_6
    };
    let mut column_1 = Column::new(50);
    
    column_1.add_group(group_1);
    column_1.add_group(group_2);

    let mut monitor_1 = Monitor::new(1, 3, 0);
    monitor_1.add_column(column_1);

    println!("windows: {}", monitor_1.count_windows());

}
