/// Window counting over the model tree.
///
/// The compositor itself never asks for a count -- these exist for the model tests
/// in `grid_tests.rs`, which assert on `count_windows` in a dozen places. dead_code
/// fires because a release build sees no caller outside the impls themselves.
#[allow(dead_code)]
pub trait CountWindows {
    fn count_windows(&self) -> usize;
}


#[allow(dead_code)]
pub fn sum_windows<T: CountWindows>(items: &[T]) -> usize {
    items.iter().map(|g| g.count_windows()).sum()
}
