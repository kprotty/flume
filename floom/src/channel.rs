// Copyright (c) 2020 kprotty
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::lock::Lock;
use std::{
    pin::Pin,
    time::{Instant, Duration},
    mem::MaybeUninit,
    future::Future,
    marker::PhantomPinned,
    ptr::NonNull,
    cell::Cell,
    task::{Task, Poll, Waker, RawWaker, RawWakerVTable, Context},
};

pub enum RecvError {
    Closed,
}

pub enum SendError {
    Closed,
}

pub enum TryRecvError {
    Empty,
    Closed,
}

pub enum TrySendError<T> {
    Full(T),
    Closed,
}

pub enum TryRecvTimeoutError {
    Closed,
    TimedOut,
}

pub enum TrySendTimeoutError<T> {
    Closed,
    TimedOut(T)
}

pub struct Channel<T>(Lock<Chan<T>>);

impl<T> Channel<T> {
    pub fn unbounded() -> Self {
        Self::with_capacity(None)
    }

    pub fn bounded(capacity: usize) -> Self {
        Self::with_capacity(Some(capacity))
    }

    fn with_capacity(capacity: Option<usize>) -> Self {
        Self(Lock::new(match capacity {
            Some(capacity) => Chan::<T>::bounded(capacity),
            None => Chan::<T>::unbounded(),
        }))
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        if let Some(waker) = self.0.with(|chan| chan.try_send(item))? {
            waker.wake();
        }
    }

    pub fn send_async(&self, item: T) -> SendFuture<'_, T> {
        SendFuture(FutureOp::new(self, item))
    }

    pub fn send(&self, item: T) -> Result<(), SendError> {
        Self::sync(None, self.send_async(item))
    }

    pub fn try_send_for(&self, item: T, timeout: Duration) -> Result<(), TrySendTimeoutError<T>> {
        self.try_send_until(item, Instant::now() + timeout)
    }

    pub fn try_send_until(&self, item: T, deadline: Instant) -> Result<(), TrySendTimeoutError<T>> {
        match Self::sync(Some(deadline), self.send_async(item)) {
            Err(send_future) => TrySendTimeoutError
        }
    }


    fn sync<F: Future>(
        deadline: Option<Instant>,
        mut future: F,
    ) -> Result<F::Output, F> {

    }


    // pub fn try_recv(&self) -> Result<T, TryRecvError> {
    //     self.0.with(|chan| chan.try_recv())
    // }

    // pub fn recv_async(&self) -> RecvFuture<'_, T> {
    //     RecvFuture::new(self)
    // }

    // pub fn recv(&self) -> Result<T, RecvError> {
    //     Self::sync(None, self.recv_async())
    // }

    // pub fn try_recv_for(&self, timeout: Duration) -> Result<T, TryRecvTimeoutError> {
    //     self.try_recv_until(Instant::now() + timeout)
    // }

    // pub fn try_recv_until(&self, deadline: Instant) -> Result<T, TryRecvTimeoutError> {
    //     Self::sync(Some(deadline), self.recv_async())
    // }   
}

pub enum FutureError {
    Closed
}

enum FutureOpState<'a, T> {
    TryOp(&'a Channel<T>, Option<T>),
    Waiting(&'a Channel<T>),
    Completed,
}

struct FutureOp<'a, T> {
    state: FutureOpState<'a, T>,
    waiter: Waiter<T>,
}

trait AsFutureOp<'a, T> {
    fn as_future_op(&self) -> &'_ FutureOp<'a, T>;
}

impl<'a, T> FutureOp<'a, T> {
    fn new(channel: &'a Channel<T>, item: Option<T>) -> Self {
        Self {
            state: FutureOpState::TryOp(channel, item),
            waiter: Waiter::from(WaiterState::Closed),
        }
    }
}

pub struct SendFuture<'a, T>(FutureOp<'a, T>);

impl<'a, T> AsFutureOp<'a, T> for SendFuture<'a, T> {
    fn as_future_op(&self) -> &'_ FutureOp<'a, T> {
        &self.0
    }
}

impl<'a, T> Drop for SendFuture<'a, T> {
    fn drop(&mut self) {
        if let FutureOpState::Waiting(channel) = self.state {
            self.try_cancel(channel);
        }
    }
}

impl<'a, T> SendFuture<'a, T> {
    #[cold]
    fn try_cancel(&self, channel: &'a Channel<T>) -> bool {
        let removed = channel.0.with(|chan| )
    }
}

impl<'a, T> Future for SendFuture<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut_self = unsafe { Pin::get_unchecked_mut(self) };
        
        match mut_self.0.state {
            FutureOpState::TryOp(channel, item) => match channel.0.with(|chan| {
                let item = item.expect("SendFuture without a item");
                let item = match chan.try_send(item) {
                    Ok(_) => return Poll::Ready(Ok()),
                    Err(TrySendError::Full(item)) => item,
                    Err(TrySendError::Closed) => return Poll::Ready(Err(SendError::Closed)),
                };
                
                let waker = ctx.waker().clone();
                mut_self.0.waiter = Waiter::from(WaiterState::Sending(waker, item));
                unsafe {
                    let pinned = Pin::new_unchecked(&mut_self.0.waiter);
                    chan.senders.push(pinned);
                }

                Poll::Pending
            }) {
                Poll::Pending => {
                    mut_self.0.state = FutureOpState::Waiting(channel);
                    Poll::Pending
                },
                Poll::Ready(output) => {
                    mut_self.0.state = FutureOpState::Completed;
                    Poll::Ready(output)
                },
            },
            FutureOpState::Waiting(_channel) => match mut_self.0.waker.poll(ctx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    mut_self.0.state = FutureOpState::Completed;
                    Poll::Ready(match mut_self.0.waiter.state.replace(WaiterState::Closed) {
                        WaiterState::Sent => Ok(()),
                        WaiterState::Closed => Err(SendError::Closed),
                        _ => unreachable!("invalid SendFuture waiter state"),
                    })
                }
            },
            FutureOpState::Completed => {
                unreachable!("SendFuture polled after completion");
            },
        }
    }
}

struct Chan<T> {
    head: usize,
    tail: usize,
    buffer: usize,
    capacity: usize,
    senders: WaiterQueue<T>,
    receivers: WaiterQueue<T>,
}

impl<T> Chan<T> {
    fn bounded(capacity: usize) -> Self {
        let capacity = capacity.min(!0usize >> 1);
        Self::with_capacity((capacity << 1) | 1);
    }

    fn unbounded() -> Self {
        Self::with_capacity(0)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            head: 0,
            tail: 0,
            buffer: 0,
            capacity,
            senders: WaiterQueue::new(),
            receivers: WaiterQueue::new(),
        }
    }

    unsafe fn try_send(&mut self, item: T) -> Result<Option<Waker>, TrySendError<T>> {
        if let Some(waiter) = self.receivers.pop() {
            match waiter.state.replace(WaiterState::Received(item)) {
                WaiterState::Receiving => return Ok(waiter.waker.wake()),
                _ => unreachable!("invalid enqueued receiver state"),
            }
        }

        // TODO:
        // capacity & 1 == bound(capacity >> 1)
        // lazy init buffer
        // buffer pointing to static MaybeUninit<T>  == closed
    }

    unsafe fn try_recv(&mut self) -> Result<(T, Option<Waker>), TryRecvError> {

    }
}

struct WaiterQueue<T> {
    head: Option<NonNull<Waiter<T>>>,
}

impl<T> WaiterQueue<T> {
    fn new() -> Self {
        Self {
            head: None,
        }
    }

    unsafe fn push(&mut self, waiter: Pin<&Waiter<T>>) {
        let waiter_ptr = NonNull::from(&*waiter);
        waiter.next.set(None);
        waiter.tail.set(waiter_ptr);

        if let Some(head) = self.head {
            let tail = head.as_ref().tail.replace(Some(waiter_ptr));
            tail.as_ref().next.set(Some(waiter_ptr));
            waiter.prev.set(Some(tail));
        } else {
            self.head = Some(waiter_ptr);
            waiter.prev.set(None);
        }
    }

    unsafe fn pop<'a>(&mut self) -> Option<Pin<&'a Waiter<T>>> {
        self.head.map(|waiter| {
            let waiter_ref = &*waiter.as_ptr();
            assert!(self.try_remove(Pin::new_unchecked(waiter_ref)));
            Pin::new_unchecked(waiter_ref)
        })
    }

    unsafe fn try_remove(&mut self, waiter: Pin<&Waiter<T>>) -> bool {
        let waiter_ptr = NonNull::from(&*waiter);
        let head = match self.head {
            Some(head) => head,
            None => return false,
        };

        let prev = waiter.prev.get();
        let next = waiter.next.get();
        if prev.is_none() && next.is_none() {
            return false;
        }

        if let Some(prev) = prev {
            prev.as_ref().next.set(next);
        }
        if let Some(next) = next {
            next.as_ref().prev.set(prev);
        }
        if head == waiter_ptr {
            self.head = next;
        }
        if head.tail.get() == Some(waiter_ptr) {
            head.tail.set(prev);
        }

        waiter.prev.set(None);
        waiter.next.set(None);
        waiter.tail.set(None);
        true
    }
}

enum WaiterState<T> {
    Closed,
    Sending(T),
    Sent,
    Receiving,
    Received(T),
}

struct Waiter<T> {
    prev: Cell<Option<NonNull<Self>>>,
    next: Cell<Option<NonNull<Self>>>,
    tail: Cell<Option<NonNull<Self>>>,
    state: Cell<WaiterState<T>>,
    waker: AtomicWaker,
    _pinned: PhantomPinned,
}

impl<T> Waiter<T> {
    fn new(state: Option<(&Context<'_>, WaiterState<T>)>) -> Self {
        let mut self = Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            tail: Cell::new(None),
            state: Cell::new(WaiterState::Closed),
            waker: AtomicWaker::new(),
            _pinned: PhantomPinned,
        };

        if let Some((state, ctx)) = state {
            self.state = Cell::new(state);
            self.waker.prepare(ctx);
        }

        self
    }
}