use std::{
    ptr::read,
    cell::Cell,
    marker::PhantomData,
    num::NonZeroUsize,
    mem::{drop, forget},
    time::{Duration, Instant},
    sync::{
        Arc,
        Mutex,
        Condvar,
        atomic::{spin_loop_hint, AtomicUsize, AtomicBool, Ordering},
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

#[repr(C)]
struct Node<T> {
    next: AtomicUsize,
    item: T,
}

struct Shared<T> {
    stub: AtomicUsize,
    head: AtomicUsize,
    tail: Cell<usize>,
    senders: AtomicUsize,
    item_count: Option<AtomicUsize>,
    send_signal: Signal,
    recv_signal: Signal,
    disconnected: AtomicBool,
    _phantom: PhantomData<T>,
}

unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        while let Ok(item) = self.try_recv() {
            drop(item);
        }
    }
}

impl<T> Shared<T> {
    pub fn new(max_items: Option<usize>) -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(Shared {
            stub: AtomicUsize::new(0),
            head: AtomicUsize::new(0),
            tail: Cell::new(0),
            senders: AtomicUsize::new(1),
            item_count: max_items.map(AtomicUsize::new),
            send_signal: Signal::new(),
            recv_signal: Signal::new(),
            disconnected: AtomicBool::new(false),
            _phantom: PhantomData,
        });
        
        let stub = &shared.stub as *const _ as usize;
        shared.tail.set(stub);
        shared.head.store(stub, Ordering::Relaxed);

        (
            Sender { shared: shared.clone() },
            Receiver { shared, _not_sync: Cell::new(()) },
        )
    }

    pub fn disconnect(&self, is_sender: bool) {
        self.disconnected.store(true, Ordering::Relaxed);
        if is_sender {
            self.recv_signal.notify();
        }
    }

    pub fn send(&self, mut item: T) -> Result<(), SendError<T>> {
        match self.try_send_inner(item, true) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => unreachable!(),
            Err(TrySendError::Disconnected(item)) => Err(SendError::Disconnected(item)),
        }
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        self.try_send_inner(item, false)
    }

    fn try_send_inner(&self, item: T, block: bool) -> Result<(), TrySendError<T>> {
        if self.disconnected.load(Ordering::Relaxed) {
            return Err(TrySendError::Disconnected(item));
        }

        if let Some(item_count) = self.item_count.as_ref() {
            let mut count = item_count.load(Ordering::Relaxed);
            loop {
                let mut spin = 0;
                while count != 0 {
                    match item_count.compare_exchange_weak(
                        count,
                        count - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(e) => count = e,
                    }
                    if spin < 6 {
                        spin += 1;
                        (0..(1 << spin)).for_each(|_| spin_loop_hint());
                    } else {
                        if cfg!(unix) {
                            std::thread::yield_now();
                        } else {
                            (0..100).for_each(|_| spin_loop_hint());
                        }
                    }
                    if self.disconnected.load(Ordering::Relaxed) {
                        return Err(TrySendError::Disconnected(item));
                    }
                }
                if !block {
                    return Err(TrySendError::Full(item));
                }
                self.send_signal.wait(None);
                if self.disconnected.load(Ordering::Relaxed) {
                    return Err(TrySendError::Disconnected(item));
                }
                count = item_count.load(Ordering::Relaxed);
            }
        }

        let node = Box::new(Node {
            next: AtomicUsize::new(0),
            item,
        });
        self.push(&node.next as *const _ as usize);
        forget(node);
        Ok(())
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        match self.try_recv_inner(Some(None)) {
            Ok(item) => Ok(item),
            Err(None) => unreachable!(),
            Err(Some(RecvTimeoutError::Timeout)) => unreachable!(),
            Err(Some(RecvTimeoutError::Disconnected)) => Err(RecvError::Disconnected),
        }
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        match self.try_recv_inner(Some(Some(deadline))) {
            Ok(item) => Ok(item),
            Err(None) => unreachable!(),
            Err(Some(error)) => Err(error),
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.try_recv_inner(None) {
            Ok(item) => Ok(item),
            Err(None) => Err(TryRecvError::Empty),
            Err(Some(RecvTimeoutError::Timeout)) => unreachable!(),
            Err(Some(RecvTimeoutError::Disconnected)) => Err(TryRecvError::Disconnected),
        }
    }

    fn try_recv_inner(&self, deadline: Option<Option<Instant>>) -> Result<T, Option<RecvTimeoutError>> {
        let mut spin = 0;
        loop {
            if self.disconnected.load(Ordering::Relaxed) {
                return Err(Some(RecvTimeoutError::Disconnected));
            }

            let mut retry = false;
            match self.pop() {
                Ok(item) => {
                    if let Some(item_count) = self.item_count.as_ref() {
                        if item_count.fetch_add(1, Ordering::Relaxed) == 0 {
                            self.send_signal.notify();
                        }
                    }
                    return Ok(item);
                },
                Err(TryRecvError::Disconnected) => retry = true,
                Err(TryRecvError::Empty) => {
                    if deadline.is_none() {
                        return Err(None);
                    }
                },
            }

            if spin <= 3 {
                spin += 1;
                (0..(1 << spin)).for_each(|_| spin_loop_hint());
            } else if retry || spin <= 10 {
                spin += 1;
                if cfg!(unix) {
                    std::thread::yield_now();
                } else {
                    (0..100).for_each(|_| spin_loop_hint());
                }
            } else if let Some(deadline) = deadline {
                let stub = &self.stub as *const _ as usize;
                if self.head.compare_exchange(stub, stub|1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                    if self.recv_signal.wait(deadline) {
                        return Err(Some(RecvTimeoutError::Timeout));
                    }
                }
            }
        }
    }

    fn push(&self, node: usize) {
        let head = self.head.swap(node, Ordering::AcqRel);
        let prev = unsafe { &*((head & !1usize) as *const Node<T>) };
        prev.next.store(node, Ordering::Release);

        if (head & 1) != 0 {
            self.recv_signal.notify();
        }
    }

    fn pop(&self) -> Result<T, TryRecvError> {
        unsafe {
            let mut tail = self.tail.get() as *const Node<T>;
            let stub = &self.stub as *const _ as *const Node<T>;
            let mut next = (&*tail).next.load(Ordering::Acquire) as *const Node<T>;

            if tail == stub {
                if next.is_null() {
                    return Err(TryRecvError::Empty);
                }
                tail = next;
                self.tail.set(tail as usize);
                next = (&*tail).next.load(Ordering::Acquire) as *const Node<T>;
            }

            if !next.is_null() {
                self.tail.set(next as usize);
                return Ok(read(&Box::from_raw(tail as *mut Node<T>).item));
            }

            let head = self.head.load(Ordering::Acquire);
            if head != (tail as usize) {
                return Err(TryRecvError::Disconnected);
            }

            self.stub.store(0, Ordering::Relaxed);
            self.push(stub as usize);
            next = (&*tail).next.load(Ordering::Acquire) as *const Node<T>;

            if !next.is_null() {
                self.tail.set(next as usize);
                return Ok(read(&Box::from_raw(tail as *mut Node<T>).item));
                
            }
            return Err(TryRecvError::Empty);
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
    _not_sync: Cell<()>,
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

