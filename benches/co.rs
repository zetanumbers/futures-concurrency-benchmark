use std::{future::Future, iter, pin::pin};

use criterion::{
    black_box, criterion_group, criterion_main, measurement::Measurement, BenchmarkGroup,
    BenchmarkId, Criterion,
};
use futures_concurrency::future::{FutureGroup, Join};
use futures_lite::{
    future::{block_on, yield_now},
    StreamExt,
};

criterion_group!(name = benches; config = Criterion::default().sample_size(10); targets = all);

criterion_main!(benches);

fn all(c: &mut Criterion) {
    for join_size in [10, 15, 18].map(|p| 2_u64.pow(p)) {
        shallow_many(
            c.benchmark_group("ready_work")
                .throughput(criterion::Throughput::Elements(join_size)),
            ready_work,
            join_size.try_into().unwrap(),
        );
        shallow_many(
            c.benchmark_group("yield_now_work")
                .throughput(criterion::Throughput::Elements(join_size)),
            yield_now_work,
            join_size.try_into().unwrap(),
        );
    }
}

async fn ready_work() -> i32 {
    1
}

async fn yield_now_work() -> i32 {
    yield_now().await;
    1
}

fn shallow_many_local<F, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F, join_size: usize)
where
    F: Fn() -> T + Copy + 'static,
    T: Future,
    M: Measurement,
{
    c.bench_function(BenchmarkId::new("seq", join_size), |b| {
        b.iter(|| {
            block_on(async {
                for _ in 0..join_size {
                    black_box(work().await);
                }
            });
        });
    })
    .bench_function(
        BenchmarkId::new("futures_concurrency::join", join_size),
        |b| {
            b.iter(|| {
                block_on(async {
                    let mut futures = Vec::with_capacity(join_size);
                    futures.resize_with(join_size, work);
                    black_box(Join::join(futures).await);
                });
            });
        },
    )
    .bench_function(
        BenchmarkId::new("futures_concurrency::FutureGroup", join_size),
        |b| {
            b.iter(|| {
                block_on(async {
                    let mut group = pin!(iter::repeat_with(work)
                        .take(join_size)
                        .collect::<FutureGroup<_>>());

                    while let Some(x) = group.next().await {
                        black_box(x);
                    }
                    black_box(&mut group);
                });
            });
        },
    )
    .bench_function(
        BenchmarkId::new("async_executor::LocalExecutor", join_size),
        |b| {
            let ex = async_executor::LocalExecutor::new();
            b.iter(|| {
                block_on(ex.run(async {
                    let mut tasks = Vec::with_capacity(join_size);
                    tasks.resize_with(join_size, || ex.spawn(work()));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        },
    )
    .bench_function(
        BenchmarkId::new("unsend::executor::Executor", join_size),
        |b| {
            let ex = unsend::executor::Executor::new();
            b.iter(|| {
                block_on(ex.run(async {
                    let mut tasks = Vec::with_capacity(join_size);
                    tasks.resize_with(join_size, || ex.spawn(work()));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        },
    );
}

fn shallow_many<F, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F, join_size: usize)
where
    F: Fn() -> T + Copy + Send + Sync + 'static,
    T: Future + Send + Sync,
    T::Output: Send,
    M: Measurement,
{
    shallow_many_local(c, work, join_size);
    c.bench_function(
        BenchmarkId::new("async_executor::Executor", join_size),
        |b| {
            let ex = async_executor::Executor::new();
            b.iter(|| {
                block_on(ex.run(async {
                    let mut tasks = Vec::with_capacity(join_size);
                    tasks.resize_with(join_size, || ex.spawn(work()));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        },
    );
}
