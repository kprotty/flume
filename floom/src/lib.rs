use std::{
    cell::UnsafeCell,
    mem::{swap, replace},
    num::NonZeroUsize,
    collections::VecDeque,
    ops::Deref,
    time::{Duration, Instant},
    sync::{
        Arc,
        Mutex,
        Condvar,
        atomic::{AtomicUsize, AtomicBool, Ordering},
    },
};

struct Signal {
    state: Mutex<usize>,
    event: Condvar,
}

impl Signal {
    const NOTIFY: usize = 1 << 0;
    const WAITER: usize = 1 << 1;

    pub fn new() -> Self {
        Self {
            state: Mutex::new(0),
            event: Condvar::new(),
        }
    }

    pub fn notify(&self) {
        let mut state = self.state.lock().unwrap();

        match *state {
            0 => *state = Self::NOTIFY,
            Self::NOTIFY => {},
            waiters => {
                *state = (waiters - Self::WAITER) + Self::NOTIFY;
                self.event.notify_one();
            },
        }
    }

    pub fn wait(&self, deadline: Option<Instant>) -> bool {
        let mut state = self.state.lock().unwrap();

        if *state == Self::NOTIFY {
            *state = 0;
            return false;
        }

        *state += Self::WAITER;
        loop {
            if *state & Self::NOTIFY != 0 {
                *state -= Self::NOTIFY;
                return false;
            }

            match deadline {
                None => {
                    state = self.event.wait(state).unwrap();
                },
                Some(deadline) => {
                    let now = Instant::now();
                    let timeout = if now >= deadline {
                        *state -= Self::WAITER;
                        return true;
                    } else {
                        deadline.duration_since(now)
                    };

                    let (new_state, timeout) = self.event.wait_timeout(state, timeout).unwrap();
                    state = new_state;
                    if timeout.timed_out() {
                        let notified = *state & Self::NOTIFY != 0;
                        if notified {
                            *state -= Self::NOTIFY;
                        } else {
                            *state -= Self::WAITER;
                        }
                        return !notified;
                    }
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SendError<T> {
    Disconnected(T),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    Disconnected,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    Shared::new(None)
}

pub fn bounded<T>(max_items: usize) -> (Sender<T>, Receiver<T>) {
    Shared::new(NonZeroUsize::new(max_items).map(|max| max.get()))
}

#[cfg_attr(target_arch = "x86", repr(align(64)))]
#[cfg_attr(target_arch = "x86_64", repr(align(128)))]
struct CachePadded<T> {
    value: T,
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

struct Inner<T> {
    queue: VecDeque<T>,
    disconnected: bool,
    reciever_waiting: bool,
}

struct Shared<T> {
    send_signal: Signal,
    recv_signal: Signal,
    senders: AtomicUsize,
    available: Option<AtomicUsize>,
    inner: UnsafeCell<Inner<T>>,
    local_queue: UnsafeCell<VecDeque<T>>,
    locked: CachePadded<AtomicBool>,
}

unsafe impl<T> Sync for Shared<T> {}

impl<T> Shared<T> {
    pub fn new(max_items: Option<usize>) -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(Shared {
            send_signal: Signal::new(),
            recv_signal: Signal::new(),
            senders: AtomicUsize::new(1),
            available: max_items.map(AtomicUsize::new),
            inner: UnsafeCell::new(Inner {
                queue: VecDeque::new(),
                disconnected: false,
                reciever_waiting: false,
            }),
            local_queue: UnsafeCell::new(VecDeque::new()),
            locked: CachePadded {
                value: AtomicBool::new(false),
            },
        });
        
        (
            Sender { shared: shared.clone() },
            Receiver { shared, _not_sync: UnsafeCell::new(()) },
        )
    }

    fn with_lock<R>(&self, f: impl FnOnce(&mut Inner<T>) -> R) -> R {
        #[inline]
        fn lock(locked: &AtomicBool) {
            if locked.compare_exchange_weak(
                false,
                true,
                Ordering::Acquire,
                Ordering::Relaxed,
            ).is_err() {
                lock_slow(locked);
            }
        }

        #[cold]
        fn lock_slow(locked: &AtomicBool) {
            #[allow(unused)]
            let mut spin: usize = 0;
            loop {
                if !locked.load(Ordering::Relaxed) {
                    if locked
                        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                    {
                        return;
                    }
                }
                if cfg!(unix) {
                    std::thread::yield_now();
                } else {
                    spin = spin.wrapping_add(1);
                    (0..spin.min(100)).for_each(|_| std::sync::atomic::spin_loop_hint());
                }
            }
        }

        lock(&self.locked);
        let result = f(unsafe { &mut *self.inner.get() });
        self.locked.store(false, Ordering::Release);
        result
    }

    pub fn disconnect(&self, is_sender: bool) {
        if self.with_lock(|inner| {
            inner.disconnected = true;
            is_sender && inner.reciever_waiting
        }) {
            self.recv_signal.notify();
        }
    }

    pub fn send(&self, item: T) -> Result<(), SendError<T>> {
        match self.try_send_inner(item, true) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(item)) => Err(SendError::Disconnected(item)),
            _ => unreachable!(),
        }
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        self.try_send_inner(item, false)
    }

    fn try_send_inner(&self, item: T, block: bool) -> Result<(), TrySendError<T>> {
        if let Some(available) = self.available.as_ref() {
            'outer: loop {
                let mut remaining = available.load(Ordering::Relaxed);
                while remaining != 0 {
                    match available.compare_exchange_weak(
                        remaining,
                        remaining - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break 'outer,
                        Err(e) => remaining = e,
                    }
                }
                if block {
                    self.send_signal.wait(None);
                } else {
                    return Err(TrySendError::Full(item));
                }
            }
        }

        match self.with_lock(|inner| {
            if inner.disconnected {
                Err(item)
            } else {
                inner.queue.push_back(item);
                Ok(replace(&mut inner.reciever_waiting, false))
            }
        }) {
            Ok(notify_receiver) => {
                if notify_receiver {
                    self.recv_signal.notify();
                }
                Ok(())
            },
            Err(item) => {
                if let Some(available) = self.available.as_ref() {
                    if available.fetch_add(1, Ordering::Relaxed) == 0 {
                        self.send_signal.notify();
                    }
                }
                Err(TrySendError::Disconnected(item))
            },
        }
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        match self.try_recv_inner(Some(None)) {
            Ok(item) => Ok(item),
            Err(Some(RecvTimeoutError::Disconnected)) => Err(RecvError::Disconnected),
            _ => unreachable!(),
        }
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        match self.try_recv_inner(Some(Some(deadline))) {
            Ok(item) => Ok(item),
            Err(Some(error)) => Err(error),
            _ => unreachable!(),
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.try_recv_inner(None) {
            Ok(item) => Ok(item),
            Err(None) => Err(TryRecvError::Empty),
            Err(Some(RecvTimeoutError::Disconnected)) => Err(TryRecvError::Disconnected),
            _ => unreachable!(),
        }
    }

    fn try_recv_inner(&self, deadline: Option<Option<Instant>>) -> Result<T, Option<RecvTimeoutError>> {
        unsafe {
            loop {
                let local_queue = &mut *self.local_queue.get();
                if let Some(item) = local_queue.pop_front() {
                    if let Some(available) = self.available.as_ref() {
                        if available.fetch_add(1, Ordering::Relaxed) == 0 {
                            self.send_signal.notify();
                        }
                    }
                    return Ok(item);
                }

                if let Err(disconnected) = self.with_lock(|inner| {
                    swap(local_queue, &mut inner.queue);
                    if local_queue.len() == 0 {
                        if inner.disconnected {
                            Err(true)
                        } else {
                            inner.reciever_waiting = deadline.is_some();
                            Err(false)
                        }
                    } else {
                        Ok(())
                    }
                }) {
                    if disconnected {
                        return Err(Some(RecvTimeoutError::Disconnected));
                    } else if let Some(deadline) = deadline {
                        if self.recv_signal.wait(deadline) {
                            return Err(Some(RecvTimeoutError::Timeout));
                        }
                    } else {
                        return Err(None);
                    }
                }
            }
        }
    }
}

pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.disconnect(true);
        }
    }
}

impl<T> Sender<T> {
    pub fn send(&self, item: T) -> Result<(), SendError<T>> {
        self.shared.send(item)
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        self.shared.try_send(item)
    }
}

pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    _not_sync: UnsafeCell<()>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.disconnect(false);
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.shared.try_recv()
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(Instant::now().checked_add(timeout).unwrap())
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(deadline)
    }

    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        Iter { receiver: self }
    }

    pub fn try_iter(&self) -> impl Iterator<Item = T> + '_ {
        TryIter { receiver: self }
    }
}

impl<T> IntoIterator for Receiver<T> {
    type Item = T;
    type IntoIter = IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { receiver: self }
    }
}

pub struct Iter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.shared.recv().ok()
    }
}

pub struct IntoIter<T> {
    receiver: Receiver<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.shared.recv().ok()
    }
}

pub struct TryIter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<'a, T> Iterator for TryIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.shared.try_recv().ok()
    }
}

