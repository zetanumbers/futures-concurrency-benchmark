use std::{future::Future, iter, pin::pin};

use criterion::{
    black_box, criterion_group, criterion_main, measurement::Measurement, BenchmarkGroup, Criterion,
};
use futures_concurrency::future::{FutureGroup, Join};
use futures_lite::{
    future::{block_on, yield_now},
    StreamExt,
};

const JOIN_SIZE: usize = 1_000_000;
const SAMPLE_SIZE: usize = 10;

criterion_group!(benches, all);

criterion_main!(benches);

fn all(c: &mut Criterion) {
    shallow_many(
        c.benchmark_group("ready_work").sample_size(SAMPLE_SIZE),
        ready_work,
    );
    shallow_many(
        c.benchmark_group("yield_now_work").sample_size(SAMPLE_SIZE),
        yield_now_work,
    );
}

async fn ready_work() -> i32 {
    1
}

async fn yield_now_work() -> i32 {
    yield_now().await;
    1
}

fn shallow_many_local<F, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F)
where
    F: Fn() -> T + Copy + 'static,
    T: Future,
    M: Measurement,
{
    c.bench_function("seq", |b| {
        b.iter(|| {
            block_on(async {
                for _ in 0..JOIN_SIZE {
                    black_box(work().await);
                }
            });
        });
    })
    .bench_function("futures_concurrency::join", |b| {
        b.iter(|| {
            block_on(async {
                let mut futures = Vec::with_capacity(JOIN_SIZE);
                futures.resize_with(JOIN_SIZE, work);
                black_box(Join::join(futures).await);
            });
        });
    })
    .bench_function("futures_concurrency::FutureGroup", |b| {
        b.iter(|| {
            block_on(async {
                let mut group = pin!(iter::repeat_with(work)
                    .take(JOIN_SIZE)
                    .collect::<FutureGroup<_>>());

                while let Some(x) = group.next().await {
                    black_box(x);
                }
                black_box(&mut group);
            });
        });
    })
    .bench_function("async_executor::LocalExecutor", |b| {
        let ex = async_executor::LocalExecutor::new();
        let mut tasks = Vec::with_capacity(JOIN_SIZE);
        b.iter(|| {
            block_on(ex.run(async {
                tasks.resize_with(JOIN_SIZE, || ex.spawn(work()));

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
                tasks.resize_with(JOIN_SIZE, || ex.spawn(work()));

                for task in tasks.drain(..) {
                    black_box(task.await);
                }
            }));
        })
    });
}

fn shallow_many<F, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F)
where
    F: Fn() -> T + Copy + Send + Sync + 'static,
    T: Future + Send + Sync,
    T::Output: Send,
    M: Measurement,
{
    shallow_many_local(c, work);
    c.bench_function("async_executor::Executor", |b| {
        let ex = async_executor::Executor::new();
        let mut tasks = Vec::with_capacity(JOIN_SIZE);
        b.iter(|| {
            block_on(ex.run(async {
                tasks.resize_with(JOIN_SIZE, || ex.spawn(work()));

                for task in tasks.drain(..) {
                    black_box(task.await);
                }
            }));
        })
    });
}
