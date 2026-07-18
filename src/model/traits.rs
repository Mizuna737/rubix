pub trait CountWindows {
    fn count_windows(&self) -> usize;
}


pub fn sum_windows<T: CountWindows>(items: &[T]) -> usize {
    items.iter().map(|g| g.count_windows()).sum()
}
