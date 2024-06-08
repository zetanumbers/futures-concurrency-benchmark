use std::future::{ready, Ready};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures_concurrency::future::{FutureGroup, Join};
use futures_lite::{future::block_on, StreamExt};

const JOIN_SIZE: usize = 1_000_000;
const SAMPLE_SIZE: usize = 10;
fn async_work<T>(x: T) -> Ready<T> {
    ready(x)
}

fn seq(c: &mut Criterion) {
    c.benchmark_group("seq")
        .sample_size(SAMPLE_SIZE)
        .bench_function("seq", |b| {
            b.iter(|| {
                block_on(async {
                    for _ in 0..JOIN_SIZE {
                        black_box(async_work(1).await);
                    }
                });
            });
        });
}

fn join(c: &mut Criterion) {
    c.benchmark_group("join")
        .sample_size(SAMPLE_SIZE)
        .bench_function("futures_concurrency::join", |b| {
            b.iter(|| {
                block_on(async {
                    let mut futures = Vec::with_capacity(JOIN_SIZE);
                    futures.resize_with(JOIN_SIZE, || async_work(1));
                    black_box(Join::join(futures).await);
                });
            });
        });
}

fn group(c: &mut Criterion) {
    c.benchmark_group("group")
        .sample_size(SAMPLE_SIZE)
        .bench_function("futures_concurrency::FutureGroup", |b| {
            let mut group = FutureGroup::with_capacity(JOIN_SIZE);
            b.iter(|| {
                block_on(async {
                    for _ in 0..JOIN_SIZE {
                        group.insert(async_work(1));
                    }

                    while let Some(x) = group.next().await {
                        black_box(x);
                    }
                    black_box(&mut group);
                });
            });
        });
}

fn spawn(c: &mut Criterion) {
    c.benchmark_group("spawn_local")
        .sample_size(SAMPLE_SIZE)
        .bench_function("async_executor::LocalExecutor", |b| {
            let ex = async_executor::LocalExecutor::new();
            let mut tasks = Vec::with_capacity(JOIN_SIZE);
            b.iter(|| {
                block_on(ex.run(async {
                    tasks.resize_with(JOIN_SIZE, || ex.spawn(async_work(1)));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        })
        .bench_function("unsend::executor::Executor", |b| {
            let ex = unsend::executor::Executor::new();
            let mut tasks = Vec::with_capacity(JOIN_SIZE);
            b.iter(|| {
                block_on(ex.run(async {
                    tasks.resize_with(JOIN_SIZE, || ex.spawn(async_work(1)));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        });
    c.benchmark_group("spawn")
        .sample_size(SAMPLE_SIZE)
        .bench_function("async_executor::Executor", |b| {
            let ex = async_executor::Executor::new();
            let mut tasks = Vec::with_capacity(JOIN_SIZE);
            b.iter(|| {
                block_on(ex.run(async {
                    tasks.resize_with(JOIN_SIZE, || ex.spawn(async_work(1)));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        });
}

criterion_group!(benches, seq, join, group, spawn);

criterion_main!(benches);
