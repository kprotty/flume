use std::{
    ptr::null,
    cell::Cell,
    ops::{Deref, DerefMut},
    collections::VecDeque,
    mem::{drop, replace, MaybeUninit},
    time::{Duration, Instant},
    sync::{
        Arc,
        Mutex,
        Condvar,
        atomic::{fence, AtomicUsize, Ordering},
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
                        return false;
                    } else {
                        deadline.duration_since(now)
                    };
                    let (new_state, timeout) = self.event.wait_timeout(state, timeout).unwrap();
                    state = new_state;
                    if timeout.timed_out() {
                        *state -= Self::WAITER;
                        let notified = *state & Self::NOTIFY != 0;
                        if notified {
                            *state -= Self::NOTIFY;
                        }
                        return notified;
                    }
                }
            }
        }
    }
}

struct Waiter {
    signal: Cell<MaybeUninit<Signal>>,
    prev: Cell<MaybeUninit<*const Self>>,
    next: Cell<MaybeUninit<*const Self>>,
    tail: Cell<MaybeUninit<*const Self>>,
}

impl Waiter {
    pub fn new() -> Self {
        Self {
            signal: Cell::new(MaybeUninit::uninit()),
            prev: Cell::new(MaybeUninit::uninit()),
            next: Cell::new(MaybeUninit::uninit()),
            tail: Cell::new(MaybeUninit::uninit()),
        }
    }

    pub fn signal(&self) -> &Signal {
        unsafe { &*(&*self.signal.as_ptr()).as_ptr() }
    }

    pub fn next(&self) -> *const Self {
        unsafe { self.prev.get().assume_init() }
    }

    pub fn push(&self, head: *const Self, init: &mut bool) -> *const Self {
        self.next.set(MaybeUninit::new(head));
        if head.is_null() {
            self.tail.set(MaybeUninit::new(self));
        } else {
            self.tail.set(MaybeUninit::new(null()));
        }

        if *init {
            *init = false;
            self.prev.set(MaybeUninit::new(null()));
            self.signal.set(MaybeUninit::new(Signal::new()));
        }

        self
    }

    pub fn find_tail<'a>(&self) -> &'a Self {
        unsafe {
            let mut tail = self.tail.get().assume_init();
            if tail.is_null() {
                let mut current = self;
                while tail.is_null() {
                    let next = &*current.next.get().assume_init();
                    next.prev.set(MaybeUninit::new(current));
                    current = next;
                    tail = current.tail.get().assume_init();
                }
                self.tail.set(MaybeUninit::new(tail));
            }
            &*tail
        }
    }
}

struct Lock<T> {
    state: AtomicUsize,
    value: Cell<T>,
}

unsafe impl<T> Send for Lock<T> {}
unsafe impl<T> Sync for Lock<T> {}

impl<T> Lock<T> {
    const MUTEX_LOCK: usize = 1 << 0;
    const QUEUE_LOCK: usize = 1 << 1;
    const QUEUE_MASK: usize = !(Self::QUEUE_LOCK | Self::MUTEX_LOCK);

    pub fn lock(&self) -> LockGuard<'_, T> {
        if self
            .state
            .compare_exchange_weak(0, Self::MUTEX_LOCK, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_slow();
        }
        LockGuard { lock: self }
    }

    #[cold]
    fn lock_slow(&self) {
        let mut spin = 0;
        let mut init = true;
        let waiter = Waiter::new();
        let mut state = self.state.load(Ordering::Relaxed);

        loop {
            if state & Self::MUTEX_LOCK == 0 {
                match self.state.compare_exchange_weak(
                    state,
                    state | Self::MUTEX_LOCK,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Err(e) => state = e,
                    Ok(_) => return,
                }
                continue;
            }

            let head = (state & Self::QUEUE_MASK) as *const Waiter;
            if spin <= 10 {
                use std::sync::atomic::spin_loop_hint;
                if spin < 6 {
                    (0..(1 << spin)).for_each(|_| spin_loop_hint());
                } else {
                    std::thread::yield_now();
                }
                spin += 1;
                state = self.state.load(Ordering::Relaxed);
                continue;
            }

            let new_state = waiter.push(head, &mut init) as usize;
            if let Err(e) = self.state.compare_exchange_weak(
                state,
                new_state | (state & !Self::QUEUE_MASK),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                state = e;
                continue;
            }

            waiter.signal().wait(None);
            waiter.prev.set(MaybeUninit::new(null()));
            spin = 0;
            state = self.state.load(Ordering::Relaxed);
        }
    }

    pub fn unlock(&self) {
        if self
            .state
            .compare_exchange(Self::MUTEX_LOCK, 0, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            self.unlock_slow();
        }
    }

    #[cold]
    fn unlock_slow(&self) {
        let mut state = self.state.fetch_sub(Self::MUTEX_LOCK, Ordering::Release);
        loop {
            if (state & Self::QUEUE_LOCK != 0) || (state & Self::QUEUE_MASK == 0) {
                return;
            }
            match self.state.compare_exchange_weak(
                state,
                state | Self::QUEUE_LOCK,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(e) => state = e,
            }
        }

        'outer: loop {
            fence(Ordering::Acquire);
            let head = unsafe { &*((state & Self::QUEUE_MASK) as *const Waiter) };
            let tail = head.find_tail();

            if state & Self::MUTEX_LOCK != 0 {
                match self.state.compare_exchange_weak(
                    state,
                    state & !Self::QUEUE_LOCK,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(e) => state = e,
                }
                continue;
            }

            let new_tail = tail.next();
            if new_tail.is_null() {
                loop {
                    match self.state.compare_exchange_weak(
                        state,
                        state & Self::MUTEX_LOCK,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(e) => state = e,
                    }
                    if state & Self::QUEUE_MASK != 0 {
                        continue 'outer;
                    }
                }
            } else {
                head.tail.set(MaybeUninit::new(new_tail));
                self.state.fetch_sub(Self::QUEUE_LOCK, Ordering::Release);
            }

            tail.signal().notify();
            return;
        }
    }
}

struct LockGuard<'a, T> {
    lock: &'a Lock<T>,
}

impl<'a, T> Drop for LockGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

impl<'a, T> Deref for LockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.as_ptr() }
    }
}

impl<'a, T> DerefMut for LockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.as_ptr() }
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
    bounded(usize::max_value())
}

pub fn bounded<T>(max_items: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        recv_signal: Signal::new(),
        send_signal: Signal::new(),
        senders: AtomicUsize::new(1),
        inner: Lock {
            state: AtomicUsize::new(0),
            value: Cell::new(Inner {
                max: max_items,
                queue: VecDeque::new(),
                disconnected: false,
                senders_waiting: 0,
                receiver_waiting: false,
            }),
        },
    });

    (
        Sender { shared: shared.clone() },
        Receiver { shared, _not_sync: Cell::new(()) },
    )
}

struct Inner<T> {
    max: usize,
    queue: VecDeque<T>,
    disconnected: bool,
    senders_waiting: usize,
    receiver_waiting: bool,
}

struct Shared<T> {
    recv_signal: Signal,
    send_signal: Signal,
    senders: AtomicUsize,
    inner: Lock<Inner<T>>,
}

impl<T> Shared<T> {
    pub fn disconnect(&self) {
        let mut inner = self.inner.lock();
        inner.disconnected = true;
    }

    pub fn send(&self, mut item: T) -> Result<(), SendError<T>> {
        loop {
            item = match self.try_send_inner(item, true) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(item)) => item,
                Err(TrySendError::Disconnected(item)) => return Err(SendError::Disconnected(item)),
            };
            self.send_signal.wait(None);
        }
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        self.try_send_inner(item, false)
    }

    fn try_send_inner(&self, item: T, wait: bool) -> Result<(), TrySendError<T>> {
        let notify_receiver = {
            let mut inner = self.inner.lock();

            if inner.disconnected {
                return Err(TrySendError::Disconnected(item));
            } else if inner.queue.len() == inner.max {
                inner.senders_waiting += if wait { 1 } else { 0 };
                return Err(TrySendError::Full(item));
            }

            inner.queue.push_back(item);
            replace(&mut inner.receiver_waiting, false)
        };
        
        if notify_receiver {
            self.recv_signal.notify();
        }
        Ok(())
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        match self.recv_inner(None) {
            Ok(item) => Ok(item),
            Err(RecvTimeoutError::Timeout) => unreachable!(),
            Err(RecvTimeoutError::Disconnected) => Err(RecvError::Disconnected),
        }
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.recv_inner(Some(deadline))
    }

    fn recv_inner(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        loop {
            match self.try_recv_inner(true) {
                Ok(item) => return Ok(item),
                Err(TryRecvError::Empty) => {},
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
            }
            if self.recv_signal.wait(deadline) {
                return Err(RecvTimeoutError::Timeout);
            }
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.try_recv_inner(false)
    }

    fn try_recv_inner(&self, wait: bool) -> Result<T, TryRecvError> {
        let (item, notify_sender) = {
            let mut inner = self.inner.lock();

            if inner.disconnected {
                return Err(TryRecvError::Disconnected);
            } else if inner.queue.len() == 0 {
                inner.receiver_waiting = wait;
                return Err(TryRecvError::Empty);
            }

            let item = inner.queue.pop_front().unwrap();
            if inner.senders_waiting == 0 {
                (item, false)
            } else {
                inner.senders_waiting -= 1;
                (item, true)
            }
        };

        if notify_sender {
            self.send_signal.notify();
        }
        Ok(item)
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
            self.shared.disconnect();
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
    _not_sync: Cell<()>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.disconnect();
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

