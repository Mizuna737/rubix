use super::geometry::{self, Rect};
use super::grid::{Monitor, Direction};


#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubeRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl CubeRect {
    pub fn left(self) -> i32 { self.x }
    pub fn right(self) -> i32 { self.x + self.width as i32 }
    pub fn top(self) -> i32 { self.y }
    pub fn bottom(self) -> i32 { self.y + self.height as i32 }
    pub fn center_x(self) -> i32 { self.x + self.width as i32 / 2 }
    pub fn center_y(self) -> i32 { self.y + self.height as i32 / 2 }
    fn from_rect(rect: Rect, row_offset: i32, band_height: u32 ) -> CubeRect {
        CubeRect {
            x: rect.x as i32,
            y: rect.y as i32 + row_offset * band_height as i32,
            width: rect.width,
            height: rect.height,
        }
    }

}

#[derive(Debug)]
pub struct CubeSlot {
    pub column: usize,
    pub row: usize,
    pub rect: CubeRect,
    pub windows: Vec<(u32, CubeRect)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubeTarget {
    pub column: usize,
    pub row: usize,
    pub window: Option<u32>,
}

pub fn find_target_by_direction(monitor: &Monitor, bounds: Rect, focused_id: Option<u32>, direction: Direction) -> Option<CubeTarget> {
    let cube = compute_cube(monitor, bounds);
    let mut candidates: Vec<(CubeTarget, CubeRect)> = Vec::new();
    for slot in &cube {
        if slot.windows.is_empty() {
            candidates.push((
                    CubeTarget { column: slot.column, row: slot.row, window: None },
                    slot.rect,
                    ));
            } else {
            for (id, rect) in &slot.windows {
                candidates.push((
                        CubeTarget { column: slot.column, row: slot.row, window: Some(*id) },
                        *rect,
                        ));
            }
        }
    }
    // resolve origin
    let origin = focused_id
        .and_then(|id| candidates.iter().find(|(t,_)| t.window == Some(id)).copied())
        .or_else(|| {
            let column = monitor.active_column();
            let row = monitor.columns().get(column)?.active_row();
            let fallback = CubeTarget { column, row, window: monitor.active_window() };
            candidates.iter().find(|(t,_)| *t == fallback).copied()
        })?;
    // exclude origin
    candidates.retain(|(t,_)| *t != origin.0);
    // nearest_in_direction
    nearest_in_direction(origin.1, &candidates, direction)
}

struct Projection { near: i32, far: i32, lo: i32, hi: i32 }

fn project(rect: CubeRect, direction: Direction) -> Projection {
    match direction {
        Direction::Right => Projection {near: rect.left(), far: rect.right(), lo: rect.top(), hi: rect.bottom()},
        Direction::Left => Projection {near: -rect.right(), far: -rect.left(), lo: rect.top(), hi: rect.bottom()},
        Direction::Down => Projection {near: rect.top(), far: rect.bottom(), lo: rect.left(), hi: rect.right()},
        Direction::Up => Projection {near: -rect.bottom(), far: -rect.top(), lo: rect.left(), hi: rect.right()},
    }
}

fn nearest_in_direction(origin: CubeRect, candidates: &[(CubeTarget, CubeRect)], direction: Direction) -> Option<CubeTarget> {
    let o = project(origin, direction);
    let overlaps = |c: &Projection| c.lo < o.hi && c.hi > o.lo;
    let minor = |c: &Projection| ((c.lo + c.hi) - (o.lo + o.hi)).abs();

    let ahead = candidates.iter()
        .map(|(t,r)| (t, project(*r, direction)))
        .filter(|(_,c)| c.near >= o.far && overlaps(c))
        .min_by_key(|(t,c)| (c.near - o.far, minor(c), t.column, t.row, t.window))
        .map(|(t,_)| *t);

    ahead.or_else(|| candidates.iter()
        .map(|(t,r)| (t, project(*r, direction)))
        .filter(|(_,c)| c.far <= o.near && overlaps(c))
        .min_by_key(|(t,c)| (std::cmp::Reverse(o.near - c.far), minor(c), t.column, t.row, t.window))
        .map(|(t,_)| *t))
}
fn column_band(monitor: &Monitor, bounds: Rect, c: usize) -> (u32, u32) {
    let visible = monitor.visible_columns().max(1);
    let width = bounds.width as usize;
    let x_left = bounds.x + (c * width / visible ) as u32;
    let x_right = bounds.x + ((c+1) * width / visible ) as u32;
    (x_left,x_right)
}

pub fn compute_cube(monitor: &Monitor, bounds: Rect) -> Vec<CubeSlot> {
    let mut slots = Vec::new();
    for (c, column) in monitor.columns().iter().enumerate() {
        let (x_left, x_right) = column_band(monitor, bounds, c);
        for (g, group) in column.groups().iter().enumerate() {
            let row_offset = g as i32 - column.active_row() as i32;

            let band = Rect {
                x: x_left,
                y: bounds.y,
                width: x_right - x_left,
                height: bounds.height,
            };

            let windows = match &group.layout {
                Some(node) => geometry::compute_layout(node, band, 0)
                    .into_iter()
                    .map(|(id, rect)| (id, CubeRect::from_rect(rect, row_offset, bounds.height)))
                    .collect(),
                None => Vec::new(),
            };

            slots.push(CubeSlot {
                column: c,
                row: g,
                rect: CubeRect::from_rect(band, row_offset, bounds.height),
                windows,
            });
        }
    }
    slots
}

#[cfg(test)] #[path = "cube_tests.rs"] mod tests;
