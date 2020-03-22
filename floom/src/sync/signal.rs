use super::Backoff;
use std::{
    time::Instant,
    sync::{
        Mutex,
        Condvar,
        atomic::{AtomicUsize, Ordering},
    },
};

const EMPTY: usize = 0 << 0;
const NOTIFIED: usize = 1 << 0;
const WAKING: usize = 1 << 1;
const WAITING: usize = 1 << 2;

pub struct Signal {
    waiters: AtomicUsize,
    permits: Mutex<usize>,
    event: Condvar,
}

impl Signal {
    pub fn new() -> Self {
        Self {
            waiters: AtomicUsize::new(EMPTY),
            permits: Mutex::new(0),
            event: Condvar::new(),
        }
    }

    pub fn notify(&self) {
        // Only store a notification if there are none currently.
        let mut waiters = self.waiters.load(Ordering::Relaxed);
        while waiters & NOTIFIED == 0 {
            let mut new_waiters = waiters | NOTIFIED;
            if waiters & WAITING != 0 {
                new_waiters |= WAKING;
            }

            // Try to store a notification and possible a wake token.
            match self.waiters.compare_exchange_weak(
                waiters,
                new_waiters,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Err(e) => waiters = e,
                Ok(EMPTY) => return,
                Ok(_) => {
                    if (waiters & WAKING == 0) && (new_waiters & WAKING != 0) {
                        self.semaphore_release();
                    }
                    return;
                },
            }
        }
    }


    /// Wait for a notification. Returns whether it timed out while waiting.
    pub fn wait(&self, deadline: Option<Instant>) -> bool {
        let mut is_waiting = false;
        let mut backoff = Backoff::new();
        let mut waiters = self.waiters.load(Ordering::Relaxed);

        loop {
            // Try to consume a notification if there is one
            if waiters & NOTIFIED != 0 {
                let mut new_waiters = waiters & !NOTIFIED;
                if is_waiting {
                    new_waiters -= WAITING;
                }
                match self.waiters.compare_exchange_weak(
                    waiters,
                    new_waiters,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return false,
                    Err(e) => waiters = e,
                }
                continue;
            }

            // Spin a little bit there there are no waiters
            // and if we haven't spun too much.
            if (waiters & WAITING == 0) && backoff.spin() {
                waiters = self.waiters.load(Ordering::Relaxed);
                continue;
            }

            // Try to transition into a waiting state to block the thread if not already
            if !is_waiting {
                match self.waiters.compare_exchange_weak(
                    waiters,
                    waiters + WAITING,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => is_waiting = true,
                    Err(e) => {
                        waiters = e;
                        continue;
                    }
                }
            }

            // Block the thread until notified
            // If timed out, mark that were not waiting anymore
            if self.semaphore_acquire(deadline) {
                self.waiters.fetch_sub(WAITING, Ordering::Relaxed);
                return true;
            }

            // A notify() thread woke us up.
            // Reset everything and try to consume the notification it left behind.
            backoff.reset();
            waiters = self.waiters.fetch_sub(WAKING, Ordering::Relaxed) - WAKING;
        }
    }

    fn semaphore_release(&self) {
        let mut permits = self.permits.lock().unwrap();
        *permits += 1;
        self.event.notify_one();
    }

    fn semaphore_acquire(&self, deadline: Option<Instant>) -> bool {
        let mut permits = self.permits.lock().unwrap();
        loop {
            if *permits > 0 {
                *permits -= 1;
                return false;
            } else if let Some(deadline) = deadline {
                let now = Instant::now();
                let timeout = if now >= deadline {
                    return true;
                } else {
                    deadline.duration_since(now)
                };
                let (new_permits, timeout) = self.event.wait_timeout(permits, timeout).unwrap();
                permits = new_permits;
                if timeout.timed_out() {
                    if *permits > 0 {
                        *permits -= 1;
                        return false;
                    } else {
                        return true;
                    }
                }
            } else {
                permits = self.event.wait(permits).unwrap();
            }
        }
    }
}