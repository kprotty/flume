use crate::lock::Lock;
use std::{
    pin::Pin,
    cell::Cell,
    mem::replace,
    iter,
    ptr::NonNull,
    future::Future,
    marker::PhantomPinned,
    task::{Poll, Context, Waker, RawWaker, RawWakerVTable},
};

pub(crate) trait Queue {
    type Item;

    fn push(&mut self, item: Self::Item) -> Result<(), Self::Item>;

    fn pop(&mut self) -> Option<Self::Item>;
}

pub(crate) type Channel<Q> = Chan<Q>;

pub(crate) struct Chan<Q: Queue> {
    queue: Option<Q>,
    senders: WaiterQueue<Q::Item>,
    receivers: WaiterQueue<Q::Item>,
}

impl<Q: Queue> Chan<Q> {
    pub fn new(queue: Q) -> Self {
        Self {
            queue: Some(queue),
            senders: WaiterQueue::new(),
            receivers: WaiterQueue::new(),
        }
    }

    pub fn close(&mut self) -> Vec<Waker> {
        self.queue = None;
        let senders = replace(&mut self.senders, WaiterQueue::new());
        let receivers = replace(&mut self.receivers, WaiterQueue::new());

        let num_waiters = senders.size + receivers.size;
        if num_waiters == 0 {
            return Vec::new();
        }

        let mut waiters = iter::from_fn(move || {
            senders.pop().or_else(|| receivers.pop())
        });
            
        let mut wakers = Vec::with_capacity(num_waiters);
        for waiter in waiters {
            wakers.push(match waiter.state.replace(WaiterState::Closed) {
                WaiterStaet::Receiving(waker) => waker,
                WaiterState::Sending(waker, _) => waker,
                _ => unreachable!("invalid waiter state"),
            });
        }
            
        wakers
    }

    pub fn try_send(&mut self, item: Q::Item) -> Result<Option<Waker>, SendError<Self::Item>> {
        if let Some(queue) = self.queue.as_mut() {
            if let Some(waiter) = self.receivers.pop() {
                match waiter.state.replace(WaiterState::Received(item)) {
                    WaiterState::Receiving(waker) => Ok(Some(waker)),
                    _ => unreachable!("invalid receiver state"),
                }
            } else if let Err(item) = queue.push(item) {
                Err(SendError::Full(item))
            } else {
                Ok(None)
            }
        } else {
            Err(SendError::Closed)
        }
    }

    pub fn try_recv(&mut self) -> Result<(Option<Waker>, Self::Item), RecvError> {
        if let Some(queue) = self.queue.as_mut() {
            if let Some(waiter) = self.senders.pop() {
                match waiter.state.replace(WaiterState::Sent) {
                    WaiterState::Sending(waker, item) => Ok((Some(waker), item)),
                    _ => unreachable!("invalid sender state"),
                }
            } else if let Some(item) = queue.pop() {
                Ok((None, item))
            } else {
                Err(RecvError::Empty)
            }
        } else {
            Err(RecvError::Closed)
        }
    }
}

pub(crate) enum SendError<T> {
    Full(T),
    Closed,
}

pub(crate) enum RecvError {
    Empty,
    Closed,
}

struct WaiterQueue<T> {
    head: Option<NonNull<Waiter<T>>>,
    size: usize,
}

impl<T> WaiterQueue<T> {
    fn new() -> Self {
        Self {
            head: None,
            size: 0,
        }
    }

    fn push(&mut self, waiter: Pin<&mut Waiter<T>>) {
        unsafe {
            let waiter_ptr = NonNull::from(&*waiter);
            waiter.next.set(None);
            waiter.tail.set(waiter_ptr);
            self.size += 1;

            if let Some(head_ptr) = self.head {
                let tail_ptr = head.as_ref().tail.get();
                tail_ptr.as_ref().next.set(Some(waiter_ptr));
                head_ptr.as_ref().tail.set(waiter_ptr);
                waiter.prev.set(Some(tail_ptr));
            } else {
                self.head = Some(waiter_ptr);
                waiter.prev.set(None);
            }
        }
    }

    fn pop<'a>(&mut self) -> Option<Pin<&'a mut Waiter<T>>> {
        self.head.map(|waiter_ptr| unsafe {
            let waiter = &mut *waiter_ptr.as_ptr();
            assert!(self.try_remove(Pin::new_unchecked(waiter)));
            Pin::new_unchecked(waiter)
        })
    }

    fn try_remove(&mut self, waiter: Pin<&mut Waiter<T>>) -> bool {
        unsafe {
            let head_ptr = match self.head {
                Some(head_ptr) => head_ptr,
                None => return false,
            };

            let prev_ptr = waiter.prev.get();
            let next_ptr = waiter.next.get();
            let tail_ptr = head_ptr.as_ref().tail.get();
            let waiter_ptr = NonNull::from(&*waiter);

            if let Some(prev_ptr) = prev_ptr {
                prev_ptr.as_ref().next.set(next_ptr);
            }
            if let Some(next_ptr) = next_ptr {
                next_ptr.as_ref().prev.set(prev_ptr);
            }

            if head_ptr == waiter_ptr {
                self.head = next_ptr;
                if let Some(new_head_ptr) = next_ptr {
                    new_head_ptr.as_ref().tail.set(tail_ptr);
                }
            } else if tail_ptr == waiter_ptr {
                if let Some(new_tail_ptr) = prev_ptr {
                    head_ptr.as_ref().tail.set(new_tail_ptr);
                }
            } else if prev_ptr.is_none() && next_ptr.is_none() {
                return false;
            }

            waiter.prev.set(None);
            waiter.next.set(None);
            waiter.tail.set(waiter_ptr);

            self.size -= 1;
            true
        }
    }
}

struct Waiter<T> {
    prev: Cell<Option<NonNull<Self>>>,
    next: Cell<Option<NonNull<Self>>>,
    tail: Cell<NonNull<Self>>,
    state: Cell<WaiterState<T>>,
    _pinned: PhantomPinned,
}

impl<T> Waiter<T> {
    fn new(state: WaiterState<T>) -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            tail: Cell::new(NonNull::dangling()),
            state: Cell::new(state),
            _pinned: PhantomPinned,
        }
    }
}

enum WaiterState<T> {
    Sending(Waker, T),
    Sent,
    Receiving(Waker),
    Received(T),
    Closed,
}

