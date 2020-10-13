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

use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    marker::PhantomPinned,
    mem::{self, MaybeUninit},
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{fence, spin_loop_hint, AtomicBool, AtomicU8, AtomicUsize, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Task, Waker},
    thread,
    time::{Duration, Instant},
};

pub enum RecvError {
    Closed,
}

pub enum SendError<T> {
    Closed(T),
}

pub enum TryRecvError {
    Empty,
    Closed,
}

pub enum TrySendError<T> {
    Full(T),
    Closed(T),
}

pub enum TryRecvTimeoutError {
    Closed,
    TimedOut,
}

pub enum TrySendTimeoutError<T> {
    Closed(T),
    TimedOut(T),
}

pub struct Channel<T>(Lock<Chan<T>>);

impl<T> Channel<T> {
    pub const fn unbounded() -> Self {
        Self::with_capacity(None)
    }

    pub const fn bounded(capacity: usize) -> Self {
        Self::with_capacity(Some(capacity))
    }

    const fn with_capacity(capacity: Option<usize>) -> Self {
        Self(Lock::new(match capacity {
            Some(capacity) => Chan::<T>::bounded(capacity),
            None => Chan::<T>::unbounded(),
        }))
    }

    pub fn is_closed(&self) -> bool {
        self.0.with(|chan| chan.is_closed())
    }

    pub fn close(&self) {
        for waker in self.0.with(|chan| chan.close()).into_iter() {
            waker.wake();
        }
    }

    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        if let Some(waker) = self.0.with(|chan| unsafe { chan.try_send(item) })? {
            waker.wake();
        }
    }

    pub fn send_async(&self, item: T) -> SendFuture<'_, T> {
        SendFuture(FutureOp::new(self, item))
    }

    pub fn send(&self, item: T) -> Result<(), SendError<T>> {
        Self::sync(None, self.send_async(item)).unwrap()
    }

    pub fn try_send_for(&self, item: T, timeout: Duration) -> Result<(), TrySendTimeoutError<T>> {
        self.try_send_until(item, Instant::now() + timeout)
    }

    pub fn try_send_until(&self, item: T, deadline: Instant) -> Result<(), TrySendTimeoutError<T>> {
        match self::sync(Some(deadline), self.send_async(item)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(SendError::Closed(item))) => Err(TrySendTimeoutError::Closed(item)),
            Err(WaiterState::Closed(item)) => Err(TrySendTimeoutError::Closed(item)),
            Err(WaiterState::Sending(item)) => Err(TrySendTimeoutError::TimedOut(item)),
            _ => unreachable!("invalid WaiterState when timed out")
        }
    }

    fn sync<F: Future>(deadline: Option<Instant>, mut future: F) -> Result<F::Output, WaiterState<T>> {
        unimplemented!("TODO")
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
    Closed,
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
            waiter: Waiter::new(),
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
            let _cancelled = self.try_cancel(channel);
        }
    }
}

impl<'a, T> SendFuture<'a, T> {
    #[cold]
    fn try_cancel(&self, channel: &'a Channel<T>) -> bool {
        channel.0.with(|chan| unsafe {
            let pinned = Pin::new_unchecked(&self.waiter);
            chan.try_remove(true, pinned)
        })
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

                let waiter = &mut_self.0.waiter;
                waiter.state.set(WaiterState::Sending(item));
                waiter.atomic_waker.prepare(ctx);

                unsafe {
                    let pinned = Pin::new_unchecked(waiter);
                    chan.senders.push(pinned);
                    Poll::Pending
                }
            }) {
                Poll::Pending => {
                    mut_self.0.state = FutureOpState::Waiting(channel);
                    Poll::Pending
                }
                Poll::Ready(output) => {
                    mut_self.0.state = FutureOpState::Completed;
                    Poll::Ready(output)
                }
            },
            FutureOpState::Waiting(_) => match mut_self.0.waiter.atomic_waker.poll(ctx) {
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
            }
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
    const fn bounded(capacity: usize) -> Self {
        let capacity = capacity.min(!0usize >> 1);
        Self::with_capacity((capacity << 1) | 1);
    }

    const fn unbounded() -> Self {
        Self::with_capacity(0)
    }

    const fn with_capacity(capacity: usize) -> Self {
        Self {
            head: 0,
            tail: 0,
            buffer: 0,
            capacity,
            senders: WaiterQueue::new(),
            receivers: WaiterQueue::new(),
        }
    }

    fn is_unbounded(&self) -> bool {
        self.capacity & 1 == 0
    }

    fn capacity(&self) -> usize {
        self.capacity >> 1
    }

    fn size(&self) -> usize {
        self.tail.wrapping_sub(self.head)
    }

    fn wrap(&self, index: usize) -> usize {
        let capacity = self.capacity();
        if capacity.is_power_of_two() {
            index & (capacity - 1)
        } else {
            index % capacity
        }
    }

    fn closed_ptr() -> usize {
        struct Global<T>(MaybeUninit<T>);

        unsafe impl Send for Global<T> {}
        unsafe impl Sync for Global<T> {}

        static CLOSED: Global<T> = Global(MaybeUninit::<T>::uninit());

        CLOSED.0.as_ptr() as usize
    }

    fn is_closed(&self) -> bool {
        self.buffer == Self::closed_ptr()
    }

    unsafe fn close(&mut self) -> WaiterQueue {
        if !self.is_closed() {
            let buffer = mem::replace(&mut self.buffer, Self::closed_ptr());
            if buffer != 0 {
                while self.size() != 0 {
                    mem::drop(self.pop());
                }
            }
        }

        let mut waiters = WaiterQueue::new();
        waiters.consume(&mut self.senders);
        waiters.consume(&mut self.receivers);
        waiters
    }

    unsafe fn try_send(&mut self, item: T) -> Result<Option<Waker>, TrySendError<T>> {
        if self.is_closed() {
            return Err(TrySendError::Closed(item));
        }

        if let Some(waiter) = self.receivers.pop() {
            match waiter.state.replace(WaiterState::Received(item)) {
                WaiterState::Receiving => return Ok(waiter.atomic_waker.wake()),
                _ => unreachable!("invalid enqueued receiver state"),
            }
        }

        let size = self.size();
        let capacity = self.capacity();
        let is_unbounded = self.capacity & 1 == 0;

        if is_unbounded {
            if capacity == 0 {
                self.grow(8);
            } else if size == capacity {
                self.grow(capacity << 1);
            }
        } else if size == capacity {
            return Err(TrySendError::Full(item));
        } else if self.buffer == 0 {
            self.grow(capacity);
        }

        self.push(item);
        Ok(None)
    }

    unsafe fn try_recv(&mut self) -> Result<(T, Option<Waker>), TryRecvError> {
        if self.is_closed() {
            return Err(TryRecvError::Closed);
        }

        if let Some(waiter) = self.senders.pop() {
            match waiter.state.replace(WaiterState::Sent) {
                WaiterState::Sending(item) => return Ok((item, waiter.atomic_waker.wake())),
                _ => unreachable!("invalid enqueued sender state"),
            }
        }

        if self.size() == 0 {
            return Err(TryRecvError::Empty);
        }

        let item = self.pop();
        Ok((item, None))
    }

    unsafe fn try_remove(&mut self, is_sender: bool, waiter: Pin<&Waiter<T>>) -> bool {
        !self.is_closed() && match is_sender {
            true => self.senders.try_remove(waiter),
            _ => self.receivers.try_remove(waiter),
        }
    }

    unsafe fn push(&mut self, item: T) {
        let index = self.wrap(self.tail);
        self.tail = self.wrap(self.tail.wrapping_add(1));

        let buffer = NonNull::<T>::new_unchecked(self.buffer);
        let item_ptr = buffer.as_ptr().add(index);
        ptr::write(item_ptr, item);
    }

    unsafe fn pop(&mut self) -> T {
        let index = self.wrap(self.head);
        self.head = self.wrap(self.head.wrapping_add(1));

        let buffer = NonNull::<T>::new_unchecked(self.buffer);
        let item_ptr = buffer.as_ptr().add(index);
        ptr::read(item_ptr)
    }

    unsafe fn grow(&mut self, new_capacity: usize) {
        use std::alloc::{alloc, dealloc, Layout};

        let new_buffer = {
            let layout = Layout::array::<T>(new_capacity).unwrap();
            alloc(layout)
        };

        let capacity = self.capacity();
        for offset in 0..capacity {
            ptr::write(new_buffer.add(offset), self.pop());
        }

        if self.buffer != 0 {
            let layout = Layout::array::<T>(capacity).unwrap();
            dealloc(self.buffer as *mut T, layout);
        }

        self.head = 0;
        self.tail = capacity;
        self.capacity = new_capacity;
        self.buffer = new_buffer as usize;
    }
}

struct WaiterQueue<T> {
    head: Option<NonNull<Waiter<T>>>,
}

impl<T> WaiterQueue<T> {
    fn new() -> Self {
        Self { head: None }
    }

    unsafe fn consume(&mut self, queue: &mut Self) {
        if let Some(queue_head) = mem::replace(&mut queue.head, None) {
            let queue_tail = queue_head
                .as_ref()
                .tail
                .get()
                .expect("queue consumed without a tail");
            if let Some(head) = self.head {
                let tail = head
                    .as_ref()
                    .tail
                    .replace(Some(queue_tail))
                    .expect("queue consuming without a tail");
                tail.as_ref().next.set(Some(queue_head));
                queue_head.as_ref().prev.set(Some(tail));
            } else {
                self.head = queue_head;
            }
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

impl<T> IntoIterator for WaiterQueue<T> {
    type Item = Waker;
    type IntoIter = ClosedIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        Self::IntoInter { head: self.head }
    }
}

struct ClosedIter<T> {
    head: Option<NonNull<Waiter<T>>>,
}

impl<T> Iterator for ClosedIter<T> {
    type Item = Waker;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(waiter) = self.head {
            let waiter = unsafe { waiter.as_ref() };
            self.head = waiter.next.get();

            match waiter.state.replace(WaiterState::Closed(None)) {
                WaiterState::Sending(item) => waiter.state.set(WaiterState::Closed(Some(item))),
                WaiterState::Receiving => {}
                _ => unreachable!("invalid waiter state when closing"),
            }

            if let Some(waker) = waiter.atomic_waker.wake() {
                return Some(waker);
            }
        }

        None
    }
}

enum WaiterState<T> {
    Closed(Option<T>),
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
    atomic_waker: AtomicWaker,
    _pinned: PhantomPinned,
}

impl<T> Waiter<T> {
    const fn new() -> Self {
        Self {
            prev: Cell::new(None),
            next: Cell::new(None),
            tail: Cell::new(None),
            state: Cell::new(WaiterState::Closed(None)),
            atomic_waker: AtomicWaker::new(),
            _pinned: PhantomPinned,
        }
    }
}

const WAKER_EMPTY: u8 = 0;
const WAKER_WAITING: u8 = 1;
const WAKER_UPDATING: u8 = 2;
const WAKER_NOTIFIED: u8 = 3;

struct AtomicWaker {
    state: AtomicU8,
    waker: Cell<Option<Waker>>,
}

unsafe impl Send for AtomicWaker {}
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            waker: Cell::new(None),
        }
    }

    fn prepare(&self, ctx: &Context<'_>) {
        self.waker.set(Some(ctx.waker().clone()));
        self.state.store(WAKER_WAITING, Ordering::Release);
    }

    fn poll(&self, ctx: &Context<'_>) -> Poll<()> {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            match state & 0b11 {
                WAKER_EMPTY => {
                    unreachable!("AtomicWaker polling before calling prepare()");
                }
                WAKER_WAITING => match self.state.compare_exchange_weak(
                    state,
                    WAKER_UPDATING,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Err(e) => state = e,
                    Ok(_) => unsafe {
                        let old_waker = self
                            .waker
                            .replace(None)
                            .expect("AtomicWaker updating without a waker");

                        self.waker.set(Some({
                            if ctx.waker().will_wake(&old_waker) {
                                old_waker
                            } else {
                                ctx.waker().clone()
                            }
                        }));

                        match self.state.compare_exchange(
                            WAKER_UPDATING,
                            WAKER_WAITING,
                            Ordering::Release,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => return Poll::Pending,
                            Err(e) => state = e,
                        }

                        assert_eq!(
                            state, WAKER_NOTIFIED,
                            "invalid AtomicWaker state when updating"
                        );
                        return Poll::Ready(());
                    },
                },
                WAKER_UPDATING => {
                    unreachable!("multiple threads polling on the same AtomicWaker");
                }
                WAKER_NOTIFIED => {
                    return Poll::Ready(());
                }
                _ => unreachable!(),
            }
        }
    }

    fn wake(&self) -> Option<Waker> {
        let state = self.state.swap(WAKER_NOTIFIED, Ordering::Acquire);
        match state & 0b11 {
            WAKER_EMPTY => {
                unreachable!("AtomicWaker woken without being prepared");
            },
            WAKER_WAITING => {
                return Some(self
                    .waker
                    .replace(None)
                    .expect("AtomicWaker waking without a Waker")
                );
            },
            WAKER_UPDATING => {
                return None; 
            },
            WAKER_NOTIFIED => {
                unreachable!("AtomicWaker woken multiple times without being re-prepared");
            },
            _ => unreachable!(),
        }
    }
}

const UNLOCKED: u8 = 0;
const LOCKED: u8 = 1;
const WAKING: usize = 256;
const WAITING: usize = !(512 - 1);

#[repr(align(512))]
struct LockWaiter {
    prev: Cell<Option<NonNull<Self>>>,
    next: Cell<Option<NonNull<Self>>>,
    tail: Cell<Option<NonNull<Self>>>,
    notified: AtomicBool,
    thread: thread::Thread,
}

struct Lock<T> {
    state: AtomicUsize,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Lock<T> {}
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(UNLOCKED as usize),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    fn with_mut<F>(&mut self, f: impl FnOnce(&mut T) -> F) -> F {
        f(unsafe { &mut *self.value.get() })
    }

    #[inline]
    fn with<F>(&self, f: impl FnOnce(&mut T) -> F) -> F {
        self.acquire();
        let result = f(unsafe { &mut *self.value.get() });
        self.release();
        result
    }

    #[inline]
    fn byte_state(&self) -> &AtomicU8 {
        unsafe { &*(&self.state as *const _ as *const _) }
    }

    #[inline]
    fn try_acquire(&self) -> bool {
        self.byte_state().swap(LOCKED, Ordering::Acquire) == UNLOCKED
    }

    #[inline]
    fn acquire(&self) {
        if !self.try_acquire() {
            self.acquire_slow();
        }
    }

    #[inline]
    fn release(&self) {
        self.byte_state().store(UNLOCKED, Ordering::Release);

        let state = self.state.load(Ordering::Relaxed);
        if (state & WAITING != 0) && (state & (WAKING | (LOCKED as usize)) == 0) {
            self.release_slow();
        }
    }

    #[cold]
    fn acquire_slow(&self) {
        let mut spin = 0;
        let waiter = Cell::new(None);
        let mut state = self.state.load(Ordering::Relaxed);

        loop {
            if state & (LOCKED as usize) == 0 {
                if self.try_acquire() {
                    return;
                }
                thread::yield_now();
                state = self.state.load(Ordering::Relaxed);
                continue;
            }

            let head = NonNull::new((state & WAITING) as *mut LockWaiter);
            if head.is_none() && spin < 10 {
                spin += 1;
                if spin <= 3 {
                    (0..(1 << spin)).for_each(|_| spin_loop_hint());
                } else {
                    thread::yield_now();
                }
                state = self.state.load(Ordering::Relaxed);
                continue;
            }

            let waiter_ref = unsafe {
                if (&*waiter.as_ptr()).is_none() {
                    waiter.set(Some(LockWaiter {
                        prev: Cell::new(None),
                        next: Cell::new(None),
                        tail: Cell::new(None),
                        notified: AtomicBool::new(false),
                        thread: thread::current(),
                    }));
                }

                (&*waiter.as_ptr()).as_ref().unwrap()
            };

            waiter_ref.next.set(head);
            waiter_ref.tail.set(match head {
                None => Some(NonNull::from(waiter_ref)),
                Some(_) => None,
            });

            if let Err(e) = self.state.compare_exchange_weak(
                state,
                (state & !WAITING) | (waiter_ref as *const _ as usize),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                state = e;
                continue;
            }

            while !waiter_ref.notified.load(Ordering::Acquire) {
                thread::park();
            }

            spin = 0;
            waiter_ref.prev.set(None);
            waiter_ref.notified.store(false, Ordering::Relaxed);
            state = self.state.fetch_sub(WAKING, Ordering::Relaxed) - WAKING;
        }
    }

    #[cold]
    fn release_slow(&self) {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if (state & WAITING == 0) || (state & (WAKING | (LOCKED as usize)) != 0) {
                return;
            }
            match self.state.compare_exchange_weak(
                state,
                state | WAKING,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break state |= WAKING,
                Err(e) => state = e,
            }
        }

        unsafe {
            'dequeue: loop {
                let head = NonNull::new((state & WAITING) as *mut LockWaiter);
                let head = head.expect("Lock waking when observed empty waiter queue");

                let tail = head.as_ref().tail.get().unwrap_or_else(|| {
                    let mut current = head;
                    loop {
                        let next = current.as_ref().next.get();
                        let next = next.expect("Lock waking observed invalid node link");
                        next.as_ref().prev.set(Some(current));
                        current = next;
                        if let Some(tail) = current.as_ref().tail.get() {
                            head.as_ref().tail.set(Some(tail));
                            break tail;
                        }
                    }
                });

                if state & (LOCKED as usize) != 0 {
                    match self.state.compare_exchange_weak(
                        state,
                        state & !WAKING,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return,
                        Err(e) => state = e,
                    }
                    fence(Ordering::Acquire);
                    continue;
                }

                match tail.as_ref().prev.get() {
                    Some(new_tail) => {
                        head.as_ref().tail.set(new_tail);
                        fence(Ordering::Release);
                    }
                    None => loop {
                        match self.state.compare_exchange_weak(
                            state,
                            (state & LOCKED) | WAKING,
                            Ordering::Release,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(e) => state = e,
                        }
                        if state & WAITING != 0 {
                            fence(Ordering::Acquire);
                            continue 'dequeue;
                        }
                    },
                }

                let waiter = tail.as_ref();
                let thread = waiter.thread.clone();
                waiter.notified.store(true, Ordering::Release);
                thread.unpark();
                return;
            }
        }
    }
}
