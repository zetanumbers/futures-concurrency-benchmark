use std::future::{ready, Ready};

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use futures_concurrency::future::{FutureGroup, Join};
use futures_lite::{future::block_on, StreamExt};

const JOIN_SIZE: usize = 1_000_000;
fn async_work<T>(x: T) -> Ready<T> {
    ready(x)
}

fn seq(c: &mut Criterion) {
    c.benchmark_group("seq")
        .sample_size(10)
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
        .sample_size(10)
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
    c.benchmark_group("group").sample_size(10).bench_function(
        "futures_concurrency::FutureGroup",
        |b| {
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
        },
    );
}

fn executor(c: &mut Criterion) {
    let ex = async_executor::LocalExecutor::new();

    c.benchmark_group("executor")
        .sample_size(10)
        .bench_function("async_executor::LocalExecutor", |b| {
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

fn unsend_executor(c: &mut Criterion) {
    let ex = unsend::executor::Executor::new();

    c.benchmark_group("executor")
        .sample_size(10)
        .bench_function("unsend::executor::Executor", |b| {
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

criterion_group!(benches, seq, join, group, executor, unsend_executor);

criterion_main!(benches);
