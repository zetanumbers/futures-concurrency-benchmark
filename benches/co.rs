use std::{
    fmt,
    future::Future,
    hash::{DefaultHasher, Hash, Hasher},
    iter,
    pin::pin,
};

use criterion::{
    async_executor::FuturesExecutor, black_box, criterion_group, criterion_main,
    measurement::Measurement, BenchmarkGroup, Criterion,
};
use futures_lite::{future::yield_now, StreamExt};
use rand::{distributions::Uniform, rngs::SmallRng, Rng, SeedableRng};
use tokio::task::{JoinSet, LocalSet};

criterion_group!(name = benches; config = Criterion::default(); targets = all);

criterion_main!(benches);

fn all(c: &mut Criterion) {
    const TASK_COUNT: u64 = 2 * 1024 * 1024;

    shallow_many(
        // TODO: tasks should be bench parameter not a group
        c.benchmark_group("ready_task")
            .throughput(criterion::Throughput::Elements(TASK_COUNT)),
        || {
            iter::repeat_with(|| async { black_box(1) })
                .take(black_box(TASK_COUNT.try_into().unwrap()))
        },
    );
    shallow_many(
        c.benchmark_group("yield_once_task")
            .throughput(criterion::Throughput::Elements(TASK_COUNT)),
        || {
            iter::repeat_with(|| async {
                yield_now().await;
                black_box(1)
            })
            .take(black_box(TASK_COUNT.try_into().unwrap()))
        },
    );
    shallow_many(
        c.benchmark_group("yield_ten_task")
            .throughput(criterion::Throughput::Elements(TASK_COUNT)),
        || {
            iter::repeat_with(|| async {
                for _ in 0..10 {
                    yield_now().await;
                }
                black_box(1)
            })
            .take(black_box(TASK_COUNT.try_into().unwrap()))
        },
    );
    shallow_many(
        c.benchmark_group("yield_hundred_task")
            .throughput(criterion::Throughput::Elements(TASK_COUNT)),
        || {
            iter::repeat_with(|| async {
                for _ in 0..100 {
                    yield_now().await;
                }
                black_box(1)
            })
            .take(black_box(TASK_COUNT.try_into().unwrap()))
        },
    );
    shallow_many(
        c.benchmark_group("yield_rand_uniform_task")
            .throughput(criterion::Throughput::Elements(TASK_COUNT)),
        || {
            let mut rng = rng_from_pkg_name();
            iter::repeat_with(move || {
                let count = black_box(rng.sample(Uniform::new(0, 100)));
                async move {
                    for _ in 0..count {
                        yield_now().await;
                    }
                    black_box(1)
                }
            })
            .take(black_box(TASK_COUNT.try_into().unwrap()))
        },
    );
}

fn rng_from_pkg_name() -> SmallRng {
    let name = env!("CARGO_PKG_NAME");
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let seed = hasher.finish();
    SmallRng::seed_from_u64(seed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeFlavor {
    CurrentThread,
    MultiThread,
}

impl fmt::Display for RuntimeFlavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeFlavor::CurrentThread => "current-thread",
            RuntimeFlavor::MultiThread => "multi-thread",
        }
        .fmt(f)
    }
}

impl RuntimeFlavor {
    fn tokio_runtime_builder(self) -> tokio::runtime::Builder {
        match self {
            RuntimeFlavor::CurrentThread => tokio::runtime::Builder::new_current_thread(),
            RuntimeFlavor::MultiThread => tokio::runtime::Builder::new_multi_thread(),
        }
    }
}

fn shallow_many_local<F, I, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F)
where
    F: Fn() -> I + Copy + 'static,
    I: Iterator<Item = T>,
    T: Future + 'static,
    M: Measurement,
{
    c.bench_function("seq", |b| {
        // TODO: make and try FuturesLiteExecutor
        b.to_async(FuturesExecutor).iter(|| async {
            for task in work() {
                black_box(task.await);
            }
        });
    })
    .bench_function("futures_concurrency::join", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            black_box(futures_concurrency::future::Join::join(work().collect::<Vec<_>>()).await);
        });
    })
    .bench_function("futures_concurrency::FutureGroup", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            let mut group = pin!(work().collect::<futures_concurrency::future::FutureGroup<_>>());

            while let Some(x) = group.next().await {
                black_box(x);
            }
        });
    })
    .bench_function("futures_util::future::JoinAll", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            black_box(work().collect::<futures_util::future::JoinAll<_>>().await);
        });
    })
    .bench_function("futures_util::stream::FuturesOrdered", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            let mut group = pin!(work().collect::<futures_util::stream::FuturesOrdered<_>>());

            while let Some(x) = group.next().await {
                black_box(x);
            }
        });
    })
    .bench_function("futures_util::stream::FuturesUnordered", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            let mut group = pin!(work().collect::<futures_util::stream::FuturesUnordered<_>>());

            while let Some(x) = group.next().await {
                black_box(x);
            }
        });
    })
    .bench_function("async_executor::LocalExecutor", |b| {
        let ex = async_executor::LocalExecutor::new();
        b.to_async(FuturesExecutor).iter(|| {
            ex.run(async {
                // FIXME: use spawn_many https://github.com/smol-rs/async-executor/pull/120
                let tasks = work().map(|t| ex.spawn(t)).collect::<Vec<_>>();
                for task in tasks {
                    black_box(task.await);
                }
            })
        })
    })
    .bench_function("unsend::executor::Executor", |b| {
        let ex = unsend::executor::Executor::new();
        b.to_async(FuturesExecutor).iter(|| {
            ex.run(async {
                let tasks = work().map(|t| ex.spawn(t)).collect::<Vec<_>>();
                for task in tasks {
                    black_box(task.await);
                }
            })
        })
    });
    for rt in [RuntimeFlavor::CurrentThread, RuntimeFlavor::MultiThread] {
        c.bench_function(format!("tokio::task::LocalSet::spawn_local/{rt}"), |b| {
            let rt = rt.tokio_runtime_builder().build().unwrap();
            b.to_async(rt).iter(|| async {
                let local_set = LocalSet::new();
                let tasks = work().map(|t| local_set.spawn_local(t)).collect::<Vec<_>>();

                local_set
                    .run_until(async {
                        for task in tasks {
                            black_box(task.await.unwrap());
                        }
                    })
                    .await;
            })
        })
        .bench_function(format!("tokio::task::JoinSet::spawn_local_on/{rt}"), |b| {
            let rt = rt.tokio_runtime_builder().build().unwrap();
            b.to_async(rt).iter(|| async {
                let local_set = LocalSet::new();
                let mut set = JoinSet::new();
                for task in work() {
                    set.spawn_local_on(task, &local_set);
                }

                local_set
                    .run_until(async {
                        while let Some(x) = set.join_next().await {
                            black_box(x.unwrap());
                        }
                    })
                    .await;
                black_box(&mut set);
            })
        });
    }
}

fn shallow_many<F, I, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F)
where
    F: Fn() -> I + Copy + Send + Sync + 'static,
    I: Iterator<Item = T> + Send + Sync,
    T: Future + Send + Sync + 'static,
    T::Output: Send,
    M: Measurement,
{
    shallow_many_local(c, work);
    c.bench_function("async_executor::Executor", |b| {
        let ex = async_executor::Executor::new();
        b.to_async(FuturesExecutor).iter(|| {
            ex.run(async {
                let mut tasks = Vec::new();
                ex.spawn_many(work(), &mut tasks);

                for task in tasks.drain(..) {
                    black_box(task.await);
                }
            })
        })
    });
    for rt in [RuntimeFlavor::CurrentThread, RuntimeFlavor::MultiThread] {
        c.bench_function(format!("tokio::task::spawn/{rt}"), |b| {
            let rt = rt.tokio_runtime_builder().build().unwrap();
            b.to_async(rt).iter(|| async {
                let tasks = work().map(tokio::task::spawn).collect::<Vec<_>>();
                for task in tasks {
                    black_box(task.await.unwrap());
                }
            })
        })
        .bench_function(format!("tokio::task::JoinSet::spawn/{rt}"), |b| {
            let rt = rt.tokio_runtime_builder().build().unwrap();
            b.to_async(rt).iter(|| async {
                let mut set = work().collect::<JoinSet<_>>();

                while let Some(x) = set.join_next().await {
                    black_box(x.unwrap());
                }
                black_box(&mut set);
            })
        });
    }
}
