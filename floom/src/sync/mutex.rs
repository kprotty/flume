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
    use crate::sync::{Backoff, CachePadded};
    use std::sync::{
        Mutex,
        MutexGuard,
        atomic::{spin_loop_hint, Ordering, AtomicBool},
    };

    pub enum Lock {
        Blocking(Mutex<()>),
        Spinning(CachePadded<AtomicBool>),
    }

    impl Lock {
        pub fn new() -> Self {
            // AMD (Ryzen) CPUs prefer std Mutex
            // Intel CPU prefer spin lock
            if Backoff::new().max_spin == 0 {
                Self::Blocking(Mutex::new(()))
            } else {
                Self::Spinning(CachePadded::new(AtomicBool::new(false)))
            }
        }

        pub fn acquire(&self) -> Option<MutexGuard<'_, ()>> {
            match self {
                Self::Blocking(mutex) => Some(mutex.lock().unwrap()),
                Self::Spinning(is_locked) => {
                    let mut spin: usize = 0;
                    loop {
                        if !is_locked.load(Ordering::Relaxed) {
                            if is_locked
                                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                                .is_ok()
                            {
                                return None;
                            }
                        }
                        spin = spin.wrapping_add(1);
                        (0..spin.min(100)).for_each(|_| spin_loop_hint());
                    }
                },
            }
        }

        pub fn release(&self, guard: Option<MutexGuard<'_, ()>>) {
            match self {
                Self::Blocking(_) => std::mem::drop(guard),
                Self::Spinning(is_locked) => is_locked.store(false, Ordering::Release),
            }
        }
    }
}