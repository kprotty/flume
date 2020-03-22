mod mutex;
mod signal;
mod backoff;

pub use mutex::Mutex;
pub use signal::Signal;
pub use backoff::Backoff;

#[cfg_attr(target_arch = "x86", repr(align(64)))]
#[cfg_attr(target_arch = "x86_64", repr(align(128)))]
struct CachePadded<T> {
    value: T,
}

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}