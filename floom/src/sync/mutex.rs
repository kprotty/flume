use self::lock_impl::Lock;
use std::{
    ops::{Deref, DerefMut},
    cell::UnsafeCell,
};

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

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.lock.acquire();
        MutexGuard { mutex: self }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

unsafe impl<'a, T> Send for MutexGuard<'a, T> {}

impl<'a, T> Drop for MutexGuard<'a, T> {
    fn drop(&mut self) {
        self.mutex.lock.release();
    }
}

impl<'a, T> DerefMut for MutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<'a, T> Deref for MutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

#[cfg(unix)]
mod lock_impl {
    use std::sync::atomic::{Ordering, AtomicBool};

    pub struct Lock {
        is_locked: AtomicBool,
    }

    impl Lock {
        pub fn new() -> Self {
            Self {
                is_locked: AtomicBool::new(false),
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

        pub fn release(&self) {
            self.is_locked.store(false, Ordering::Release);
        }
    }
}

#[cfg(windows)]
mod lock_impl {
    use std::sync::atomic::{spin_loop_hint, Ordering, AtomicBool};

    pub struct Lock {
        is_locked: AtomicBool,
    }

    impl Lock {
        pub fn new() -> Self {
            Self {
                is_locked: AtomicBool::new(false),
            }
        }

        pub fn acquire(&self) {
            let mut spin: usize = 0;
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
                spin = spin.wrapping_add(1);
                for _ in 0..spin.min(100) {
                    spin_loop_hint();
                }
            }
        }

        pub fn release(&self) {
            self.is_locked.store(false, Ordering::Release);
        }
    }
}