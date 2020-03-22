use self::lock_impl::Lock;
use std::cell::UnsafeCell;

pub struct Mutex<T> {
    lock: Lock,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            lock: Lock::new(),
            value: UnsafeCell::new(value),
        }
    }

    pub fn locked<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let guard = self.lock.acquire();
        let result = f(unsafe { &mut *self.value.get() });
        self.lock.release(guard);
        result
    }
}

#[cfg(unix)]
mod lock_impl {
    use crate::sync::CachePadded;
    use std::sync::atomic::{Ordering, AtomicBool};

    pub struct Lock {
        is_locked: CachePadded<AtomicBool>,
    }

    impl Lock {
        pub fn new() -> Self {
            Self {
                is_locked: CachePadded::new(AtomicBool::new(false)),
            }
        }

        pub fn acquire(&self) {
            loop {
                if !self.is_locked.load(Ordering::Relaxed) {
                    if self
                        .is_locked
                        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                    {
                        return;
                    }
                }
                std::thread::yield_now();
            }
        }

        pub fn release(&self, _guard: ()) {
            self.is_locked.store(false, Ordering::Release);
        }
    }
}

#[cfg(windows)]
mod lock_impl {
    use std::sync::{Mutex, MutexGuard};

    pub struct Lock {
        inner: Mutex<()>,
    }

    impl Lock {
        pub fn new() -> Self {
            Self {
                inner: Mutex::new(()),
            }
        }

        pub fn acquire(&self) -> MutexGuard<'_, ()> {
            self.inner.lock().unwrap()
        }

        pub fn release(&self, guard: MutexGuard<'_, ()>) {
            std::mem::drop(guard);
        }
    }
}