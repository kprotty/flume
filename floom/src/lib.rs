mod sync;
use sync::{Mutex, Signal};

use std::{
    cell::{RefCell, UnsafeCell},
    collections::VecDeque,
    time::{Duration, Instant},
    sync::{Arc, atomic::{Ordering, AtomicUsize}},
};

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

pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    Shared::new(Some(capacity))
}

struct Inner<T> {
    queue: VecDeque<T>,
    disconnected: bool,
}

struct Shared<T> {
    senders: AtomicUsize,
    inner: Mutex<Inner<T>>,
    recv_signal: Signal,
    send_signal: Signal,
    capacity: Option<usize>,
}

unsafe impl<T> Sync for Shared<T> {}

impl<T> Shared<T> {
    pub fn new(capacity: Option<usize>) -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(Shared {
            senders: AtomicUsize::new(1),
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                disconnected: false,
            }),
            recv_signal: Signal::new(),
            send_signal: Signal::new(),
            capacity: capacity,
        });
        
        (
            Sender { shared: shared.clone() },
            Receiver { 
                shared,
                local_queue: RefCell::new(VecDeque::new()),
                _not_sync: UnsafeCell::new(()),
            },
        )
    }

    pub fn disconnect(&self, is_sender: bool) {
        self.inner.locked(|inner| inner.disconnected = true);
        if is_sender {
            self.recv_signal.notify();
        } else {
            self.send_signal.notify();
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

    pub fn recv(&self, local_queue: &mut VecDeque<T>) -> Result<T, RecvError> {
        match self.try_recv_inner(local_queue, None, true) {
            Ok(item) => Ok(item),
            Err(Some(RecvTimeoutError::Disconnected)) => Err(RecvError::Disconnected),
            _ => unreachable!(),
        }
    }

    pub fn recv_deadline(&self, local_queue: &mut VecDeque<T>, deadline: Instant) -> Result<T, RecvTimeoutError> {
        match self.try_recv_inner(local_queue, Some(deadline), true) {
            Ok(item) => Ok(item),
            Err(Some(error)) => Err(error),
            _ => unreachable!(),
        }
    }

    pub fn try_recv(&self, local_queue: &mut VecDeque<T>) -> Result<T, TryRecvError> {
        match self.try_recv_inner(local_queue, None, false) {
            Ok(item) => Ok(item),
            Err(None) => Err(TryRecvError::Empty),
            Err(Some(RecvTimeoutError::Disconnected)) => Err(TryRecvError::Disconnected),
            _ => unreachable!(),
        }
    }

    fn try_send_inner(&self, mut item: T, block: bool) -> Result<(), TrySendError<T>> {
        let capacity = self.capacity.unwrap_or(usize::max_value());
        loop {
            item = match self.inner.locked(|inner| {
                if inner.disconnected || inner.queue.len() == capacity {
                    Err((inner.disconnected, item))
                } else {
                    Ok(inner.queue.push_back(item))
                }
            }) {
                Ok(()) => {
                    self.recv_signal.notify();
                    return Ok(());
                },
                Err((disconnected, item)) => {
                    if disconnected {
                        return Err(TrySendError::Disconnected(item))
                    } else if block {
                        self.send_signal.wait(None);
                        item
                    } else {
                        return Err(TrySendError::Full(item));
                    }
                }
            }
        }
    }

    fn try_recv_inner(
        &self,
        local_queue: &mut VecDeque<T>,
        deadline: Option<Instant>,
        block: bool,
    ) -> Result<T, Option<RecvTimeoutError>> {
        let is_bounded = self.capacity.is_some();
        loop {
            if let Some(item) = local_queue.pop_front() {
                self.send_signal.notify();
                return Ok(item);
            }

            match self.inner.locked(|inner| {
                if is_bounded {
                    if let Some(item) = inner.queue.pop_front() {
                        Ok(Some(item))
                    } else {
                        Err(inner.disconnected)
                    }
                } else {
                    if inner.queue.len() > 0 {
                        std::mem::swap(&mut inner.queue, local_queue);
                        Ok(None)
                    } else {
                        Err(inner.disconnected)
                    }
                }
            }) {
                Ok(None) => {},
                Ok(Some(item)) => {
                    self.send_signal.notify();
                    return Ok(item);
                },
                Err(disconnected) => {
                    if disconnected {
                        return Err(Some(RecvTimeoutError::Disconnected));
                    } else if block {
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
    local_queue: RefCell<VecDeque<T>>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.disconnect(false);
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.shared.try_recv(&mut self.local_queue.borrow_mut())
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.recv(&mut self.local_queue.borrow_mut())
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_deadline(Instant::now().checked_add(timeout).unwrap())
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.shared.recv_deadline(&mut self.local_queue.borrow_mut(), deadline)
    }

    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        struct Iter<'a, T> {
            receiver: &'a Receiver<T>,
        }

        impl<'a, T> Iterator for Iter<'a, T> {
            type Item = T;

            fn next(&mut self) -> Option<Self::Item> {
                self.receiver.recv().ok()
            }
        }

        Iter { receiver: self }
    }

    pub fn try_iter(&self) -> impl Iterator<Item = T> + '_ {
        struct TryIter<'a, T> {
            receiver: &'a Receiver<T>,
        }

        impl<'a, T> Iterator for TryIter<'a, T> {
            type Item = T;

            fn next(&mut self) -> Option<Self::Item> {
                self.receiver.try_recv().ok()
            }
        }

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

pub struct IntoIter<T> {
    receiver: Receiver<T>,
}

impl<T> Iterator for IntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}



 