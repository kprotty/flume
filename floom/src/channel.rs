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
        Self::with_buffer(Buffer::Unbounded({
            Vec::new().into_boxed_slice()
        }))
    }

    pub fn bounded(capacity: usize) -> Self {
        Self::with_buffer(Buffer::Bounded({
            let mut buf = Vec::with_capacity(0);
            (0..capacity).for_each(|| buf.push(MaybeUninit::uninit()));
            buf.into_boxed_slice()
        }))
    }

    fn with_buffer(buffer: Buffer<T>) -> Self {
        Self(Lock::new(Chan {
            head: 0,
            tail: 0,
            buffer,
            senders: WaiterQueue::new(),
            receivers: WaiterQueue::new(),
        }))
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        self.0.with(|chan| chan.try_send(item))
    }

    pub fn send_async(&self, item: T) -> SendFuture<'_, T> {
        SendFuture::new(self, item)
    }

    pub fn send(&self, item: T) -> Result<(), SendError> {
        match self.send_sync(None, item) {

        }
    }

    pub fn try_send_for(&self, item: T, timeout: Duration) -> Result<(), TrySendTimeoutError<T>> {
        self.try_send_until(item, Instant::now() + timeout)
    }

    pub fn try_send_until(&self, item: T, deadline: Instant) -> Result<(), TrySendTimeoutError<T>> {
        Self::sync(Some(deadline), self.send_async(item))
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.0.with(|chan| unsafe { chan.try_recv() })
    }

    pub fn recv_async(&self) -> RecvFuture<'_, T> {
        RecvFuture::new(self)
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        Self::sync(None, self.recv_async())
    }

    pub fn try_recv_for(&self, timeout: Duration) -> Result<T, TryRecvTimeoutError> {
        self.try_recv_until(Instant::now() + timeout)
    }

    pub fn try_recv_until(&self, deadline: Instant) -> Result<T, TryRecvTimeoutError> {
        Self::sync(Some(deadline), self.recv_async())
    }

    fn send_sync<T>() {
        self.sync(
            None,
            item,
            |item, chan, waker_fn| match chan.try_send(item) {
                Ok(_) => Ok(Ok()),
                Err(TrySendError::Closed) => Ok(Err(SendError::Closed)),
                Err(TrySendError::Full(item)) => Err(WaiterState::Sending(waker_fn(), item)),
            },
            |waiter_state| match waiter_state {
                WaiterState::Closed => Err(SendError::Closed),
                WaiterState::Sent => Ok(()),
                _ => unreachable!("invalid send waiter_state"),
            } 
        )
    }

    fn sync<I>(
        &self,
        deadline: Option<Instant>, mut future: F) -> F::Output {

    }
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
    waker: AtomicWaker,
    waiter: Waiter<T>,
}

pub struct SendFuture<'a, T>(FutureOp<'a, T>);

impl<'a, T> Future for SendFuture<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut_self = unsafe { Pin::get_unchecked_mut(self) };
        match mut_self.0.state {
            FutureOpState::TryOp(channel, item) => match channel.0.with(|chan| unsafe {
                let item = item.expect("SendFuture without a item");
                let item = match chan.try_send(item) {
                    Ok(_) => return Poll::Ready(Ok()),
                    Err(SendError::Full(item)) => item,
                    Err(SendError::Closed) => return Poll::Ready(Err(SendError::Closed)),
                    _ => unreachable!("invalid try_send() item"),
                };
                
                let waker = ctx.waker().clone();
                mut_self.0.waiter.state.set(WaiterState::Sending(waker, item));
                self.senders.push(&mut_self.0.waiter); 
                Poll::Pending
            }) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(output) => {
                    mut_self.0.state = FutureOpState::Completed;
                    Poll::Ready(output)
                },
            },
            FutureOpState::Waiting(channel) => match mut_self.0.waker.poll(ctx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    mut_self.0.state = FutureOpState::Completed;
                    Poll::Ready(match mut_self.0.waiter.state.replace(WaiterState::Closed) {
                        WaiterState::Closed => Err(SendError::Closed),
                        WaiterState::Sent => Ok(())
                    })
                }
            },
            FutureOpState::Completed => {
                unreachable!("SendFuture polled after completion");
            },
        }
    }
}

enum Buffer<T> {
    Closed,
    Bounded(Box<[MaybeUninit<T>]>),
    Unbounded(Box<[MaybeUninit<T>]>),
}

struct Chan<T> {
    head: usize,
    tail: usize,
    buffer: Buffer<T>,
    senders: WaiterQueue<T>,
    receivers: WaiterQueue<T>,
}

impl<T> Chan<T> {
    unsafe fn try_send(&mut self, item: T) -> Result<(), SendError<T>> {

    }

    unsafe fn try_recv(&mut self) -> Result<T, RecvError> {

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

    unsafe fn push(&mut self, waiter: &Waiter<T>) {

    }

    unsafe fn pop<'a>(&mut self) -> Option<&'a Waiter<T>> {

    }

    unsafe fn try_remove(&mut self, waiter: &Waiter<T>) -> bool {

    }
}

enum WaiterState<T> {
    Closed,
    Sending(Waker, T),
    Sent,
    Receiving(Waker),
    Received(T),
}

struct Waiter<T> {
    prev: Cell<Option<NonNull<Self>>>,
    next: Cell<Option<NonNull<Self>>>,
    tail: Cell<Option<NonNull<Self>>>,
    state: Cell<WaiterState<T>>,
    _pinned: PhantomPinned,
}

impl<T> From<WaiterState<T>> for Waiter<T> {
    fn from(state: WaiterState<T>) -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            tail: Cell::new(None),
            state: Cell::new(state),
            _pinned: PhantomPinned,
        }
    }
}