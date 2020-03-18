#[macro_use]
extern crate criterion;

use std::{
    sync::mpsc,
    thread,
    fmt::Debug,
};
use criterion::{Criterion, Bencher, black_box};

trait Sender: Clone + Send + Sized + 'static {
    type Item: Debug + Default;
    type BoundedSender: Sender<Item=Self::Item>;
    type Receiver: Receiver<Item=Self::Item>;

    fn unbounded() -> (Self, Self::Receiver);
    fn bounded(n: usize) -> (Self::BoundedSender, Self::Receiver);
    fn send(&self, msg: Self::Item);
}

trait Receiver: Send + Sized + 'static {
    type Item: Default;
    fn recv(&self) -> Self::Item;
    fn iter(&self) -> Box<dyn Iterator<Item=Self::Item> + '_>;
}

impl<T: Send + Debug + Default + 'static> Sender for flume::Sender<T> {
    type Item = T;
    type BoundedSender = Self;
    type Receiver = flume::Receiver<T>;

    fn unbounded() -> (Self, Self::Receiver) {
        flume::unbounded()
    }

    fn bounded(n: usize) -> (Self::BoundedSender, Self::Receiver) {
        flume::bounded(n)
    }

    fn send(&self, msg: T) {
        flume::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for flume::Receiver<T> {
    type Item = T;

    fn recv(&self) -> Self::Item {
        flume::Receiver::recv(self).unwrap()
    }

    fn iter(&self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(flume::Receiver::iter(self))
    }
}

impl<T: Send + Debug + Default + 'static> Sender for floom::Sender<T> {
    type Item = T;
    type BoundedSender = Self;
    type Receiver = floom::Receiver<T>;

    fn unbounded() -> (Self, Self::Receiver) {
        floom::unbounded()
    }

    fn bounded(n: usize) -> (Self::BoundedSender, Self::Receiver) {
        floom::bounded(n)
    }

    fn send(&self, msg: T) {
        floom::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for floom::Receiver<T> {
    type Item = T;

    fn recv(&self) -> Self::Item {
        floom::Receiver::recv(self).unwrap()
    }

    fn iter(&self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(floom::Receiver::iter(self))
    }
}

impl<T: Send + Debug + Default + 'static> Sender for crossbeam_channel::Sender<T> {
    type Item = T;
    type BoundedSender = Self;
    type Receiver = crossbeam_channel::Receiver<T>;

    fn unbounded() -> (Self, Self::Receiver) {
        crossbeam_channel::unbounded()
    }

    fn bounded(n: usize) -> (Self::BoundedSender, Self::Receiver) {
        crossbeam_channel::bounded(n)
    }

    fn send(&self, msg: T) {
        crossbeam_channel::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for crossbeam_channel::Receiver<T> {
    type Item = T;

    fn recv(&self) -> Self::Item {
        crossbeam_channel::Receiver::recv(self).unwrap()
    }

    fn iter(&self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(crossbeam_channel::Receiver::iter(self))
    }
}

impl<T: Send + Debug + Default + 'static> Sender for mpsc::Sender<T> {
    type Item = T;
    type BoundedSender = mpsc::SyncSender<T>;
    type Receiver = mpsc::Receiver<T>;

    fn unbounded() -> (Self, Self::Receiver) {
        mpsc::channel()
    }

    fn bounded(n: usize) -> (Self::BoundedSender, Self::Receiver) {
        mpsc::sync_channel(n)
    }

    fn send(&self, msg: T) {
        mpsc::Sender::send(self, msg).unwrap();
    }
}

impl<T: Send + Debug + Default + 'static> Sender for mpsc::SyncSender<T> {
    type Item = T;
    type BoundedSender = Self;
    type Receiver = mpsc::Receiver<T>;

    fn unbounded() -> (Self, Self::Receiver) { unimplemented!() }
    fn bounded(_: usize) -> (Self::BoundedSender, Self::Receiver) { unimplemented!() }

    fn send(&self, msg: T) {
        mpsc::SyncSender::send(self, msg).unwrap();
    }
}

impl<T: Send + Default + 'static> Receiver for mpsc::Receiver<T> {
    type Item = T;

    fn recv(&self) -> Self::Item {
        mpsc::Receiver::recv(self).unwrap()
    }

    fn iter(&self) -> Box<dyn Iterator<Item=T> + '_> {
        Box::new(mpsc::Receiver::iter(self))
    }
}

fn test_create<S: Sender>(b: &mut Bencher) {
    b.iter(|| S::unbounded());
}

fn test_oneshot<S: Sender>(b: &mut Bencher) {
    b.iter(|| {
        let (tx, rx) = S::unbounded();
        tx.send(Default::default());
        black_box(rx.recv());
    });
}

fn test_inout<S: Sender>(b: &mut Bencher) {
    let (tx, rx) = S::unbounded();
    b.iter(|| {
        tx.send(Default::default());
        black_box(rx.recv());
    });
}

fn test_hydra<S: Sender>(b: &mut Bencher, thread_num: usize, msg_num: usize) {
    let (main_tx, main_rx) = S::unbounded();

    let mut txs = Vec::new();
    for _ in 0..thread_num {
        let main_tx = main_tx.clone();
        let (tx, rx) = S::unbounded();
        txs.push(tx);

        thread::spawn(move || {
            for msg in rx.iter() {
                main_tx.send(msg);
            }
        });
    }

    drop(main_tx);

    b.iter(|| {
        for tx in &txs {
            for _ in 0..msg_num {
                tx.send(Default::default());
            }
        }

        for _ in 0..thread_num {
            for _ in 0..msg_num {
                black_box(main_rx.recv());
            }
        }
    });
}

fn test_robin_u<S: Sender>(b: &mut Bencher, thread_num: usize, msg_num: usize) {
    let (mut main_tx, main_rx) = S::unbounded();

    for _ in 0..thread_num {
        let (mut tx, rx) = S::unbounded();
        std::mem::swap(&mut tx, &mut main_tx);

        thread::spawn(move || {
            for msg in rx.iter() {
                tx.send(msg);
            }
        });
    }

    b.iter(|| {
        for _ in 0..msg_num {
            main_tx.send(Default::default());
        }

        for _ in 0..msg_num {
            black_box(main_rx.recv());
        }
    });
}

fn test_robin_b<S: Sender>(b: &mut Bencher, thread_num: usize, msg_num: usize) {
    let (mut main_tx, main_rx) = S::bounded(1);

    for _ in 0..thread_num {
        let (mut tx, rx) = S::bounded(1);
        std::mem::swap(&mut tx, &mut main_tx);

        thread::spawn(move || {
            for msg in rx.iter() {
                tx.send(msg);
            }
        });
    }

    b.iter(|| {
        let main_tx = main_tx.clone();
        thread::spawn(move || {
            for _ in 0..msg_num {
                main_tx.send(Default::default());
            }
        });

        for _ in 0..msg_num {
            black_box(main_rx.recv());
        }
    });
}

fn create(b: &mut Criterion) {
    b.bench_function("create-flume", |b| test_create::<flume::Sender<u32>>(b));
    b.bench_function("create-floom", |b| test_create::<floom::Sender<u32>>(b));
    b.bench_function("create-crossbeam", |b| test_create::<crossbeam_channel::Sender<u32>>(b));
    b.bench_function("create-std", |b| test_create::<mpsc::Sender<u32>>(b));
}

fn oneshot(b: &mut Criterion) {
    b.bench_function("oneshot-flume", |b| test_oneshot::<flume::Sender<u32>>(b));
    b.bench_function("oneshot-floom", |b| test_oneshot::<floom::Sender<u32>>(b));
    b.bench_function("oneshot-crossbeam", |b| test_oneshot::<crossbeam_channel::Sender<u32>>(b));
    b.bench_function("oneshot-std", |b| test_oneshot::<mpsc::Sender<u32>>(b));
}

fn inout(b: &mut Criterion) {
    b.bench_function("inout-flume", |b| test_inout::<flume::Sender<u32>>(b));
    b.bench_function("inout-floom", |b| test_inout::<floom::Sender<u32>>(b));
    b.bench_function("inout-crossbeam", |b| test_inout::<crossbeam_channel::Sender<u32>>(b));
    b.bench_function("inout-std", |b| test_inout::<mpsc::Sender<u32>>(b));
}

fn hydra_32t_1m(b: &mut Criterion) {
    b.bench_function("hydra-32t-1m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 32, 1));
    b.bench_function("hydra-32t-1m-floom", |b| test_hydra::<floom::Sender<u32>>(b, 32, 1));
    b.bench_function("hydra-32t-1m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 32, 1));
    b.bench_function("hydra-32t-1m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 32, 1));
}

fn hydra_32t_1000m(b: &mut Criterion) {
    b.bench_function("hydra-32t-1000m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 32, 1000));
    b.bench_function("hydra-32t-1000m-floom", |b| test_hydra::<floom::Sender<u32>>(b, 32, 1000));
    b.bench_function("hydra-32t-1000m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 32, 1000));
    b.bench_function("hydra-32t-1000m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 32, 1000));
}

fn hydra_1t_1000m(b: &mut Criterion) {
    b.bench_function("hydra-1t-1000m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 1, 1000));
    b.bench_function("hydra-1t-1000m-floom", |b| test_hydra::<floom::Sender<u32>>(b, 1, 1000));
    b.bench_function("hydra-1t-1000m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 1, 1000));
    b.bench_function("hydra-1t-1000m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 1, 1000));
}

fn hydra_4t_10000m(b: &mut Criterion) {
    b.bench_function("hydra-4t-10000m-flume", |b| test_hydra::<flume::Sender<u32>>(b, 4, 10000));
    b.bench_function("hydra-4t-10000m-floom", |b| test_hydra::<floom::Sender<u32>>(b, 4, 10000));
    b.bench_function("hydra-4t-10000m-crossbeam", |b| test_hydra::<crossbeam_channel::Sender<u32>>(b, 4, 10000));
    b.bench_function("hydra-4t-10000m-std", |b| test_hydra::<mpsc::Sender<u32>>(b, 4, 10000));
}

fn robin_u_32t_1m(b: &mut Criterion) {
    b.bench_function("robin-u-32t-1m-flume", |b| test_robin_u::<flume::Sender<u32>>(b, 32, 1));
    b.bench_function("robin-u-32t-1m-floom", |b| test_robin_u::<floom::Sender<u32>>(b, 32, 1));
    b.bench_function("robin-u-32t-1m-crossbeam", |b| test_robin_u::<crossbeam_channel::Sender<u32>>(b, 32, 1));
    b.bench_function("robin-u-32t-1m-std", |b| test_robin_u::<mpsc::Sender<u32>>(b, 32, 1));
}

fn robin_u_4t_1000m(b: &mut Criterion) {
    b.bench_function("robin-u-4t-1000m-flume", |b| test_robin_u::<flume::Sender<u32>>(b, 4, 1000));
    b.bench_function("robin-u-4t-1000m-floom", |b| test_robin_u::<floom::Sender<u32>>(b, 4, 1000));
    b.bench_function("robin-u-4t-1000m-crossbeam", |b| test_robin_u::<crossbeam_channel::Sender<u32>>(b, 4, 1000));
    b.bench_function("robin-u-4t-1000m-std", |b| test_robin_u::<mpsc::Sender<u32>>(b, 4, 1000));
}

fn robin_b_32t_16m(b: &mut Criterion) {
    b.bench_function("robin-b-32t-16m-flume", |b| test_robin_b::<flume::Sender<u32>>(b, 32, 16));
    b.bench_function("robin-b-32t-16m-floom", |b| test_robin_b::<floom::Sender<u32>>(b, 32, 16));
    b.bench_function("robin-b-32t-16m-crossbeam", |b| test_robin_b::<crossbeam_channel::Sender<u32>>(b, 32, 16));
    b.bench_function("robin-b-32t-16m-std", |b| test_robin_b::<mpsc::Sender<u32>>(b, 32, 16));
}

fn robin_b_4t_1000m(b: &mut Criterion) {
    b.bench_function("robin-b-4t-1000m-flume", |b| test_robin_b::<flume::Sender<u32>>(b, 4, 1000));
    b.bench_function("robin-b-4t-1000m-floom", |b| test_robin_b::<flume::Sender<u32>>(b, 4, 1000));
    b.bench_function("robin-b-4t-1000m-crossbeam", |b| test_robin_b::<crossbeam_channel::Sender<u32>>(b, 4, 1000));
    b.bench_function("robin-b-4t-1000m-std", |b| test_robin_b::<mpsc::Sender<u32>>(b, 4, 1000));
}

criterion_group!(
    compare,
    //create,
    //oneshot,
    //inout,
    hydra_32t_1m,
    hydra_32t_1000m,
    hydra_1t_1000m,
    hydra_4t_10000m,
    robin_u_32t_1m,
    robin_u_4t_1000m,
    robin_b_32t_16m,
    robin_b_4t_1000m,
);
criterion_main!(compare);
