/// Aligns a thread slot to 128 bytes so neighboring threads do not share a
/// cache line for their atomics. On a typical 64-byte x86 line that is a
/// twofold margin: a domain slot is wider than one line and does not sit
/// against its neighbor because of prefetch.
#[repr(C, align(128))]
pub struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }
}

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}
