use std::{fmt, future::Future, iter, pin::pin};

use criterion::{
    async_executor::FuturesExecutor, black_box, criterion_group, criterion_main,
    measurement::Measurement, BenchmarkGroup, BenchmarkId, Criterion,
};
use futures_concurrency::future::{FutureGroup, Join};
use futures_lite::{future::yield_now, StreamExt};
use tokio::task::{JoinSet, LocalSet};

criterion_group!(name = benches; config = Criterion::default().sample_size(10); targets = all);

criterion_main!(benches);

fn all(c: &mut Criterion) {
    for task_count in [10, 15, 18].map(|p| 2_u64.pow(p)) {
        shallow_many(
            c.benchmark_group("ready_task")
                .throughput(criterion::Throughput::Elements(task_count)),
            ready_task,
            task_count.try_into().unwrap(),
        );
        shallow_many(
            c.benchmark_group("yield_now_task")
                .throughput(criterion::Throughput::Elements(task_count)),
            yield_now_task,
            task_count.try_into().unwrap(),
        );
    }
}

async fn ready_task() -> i32 {
    1
}

async fn yield_now_task() -> i32 {
    yield_now().await;
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokioParams {
    tasks: usize,
    rt: TokioRuntimeFlavor,
}

impl fmt::Display for TokioParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokioRuntimeFlavor {
    CurrentThread,
    MultiThread,
}

impl TokioRuntimeFlavor {
    fn builder(&self) -> tokio::runtime::Builder {
        match self {
            TokioRuntimeFlavor::CurrentThread => tokio::runtime::Builder::new_current_thread(),
            TokioRuntimeFlavor::MultiThread => tokio::runtime::Builder::new_multi_thread(),
        }
    }
}

impl fmt::Display for TokioRuntimeFlavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokioRuntimeFlavor::CurrentThread => "current_thread",
            TokioRuntimeFlavor::MultiThread => "multi_thread",
        }
        .fmt(f)
    }
}

fn shallow_many_local<F, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F, task_count: usize)
where
    F: Fn() -> T + Copy + 'static,
    T: Future + 'static,
    M: Measurement,
{
    c.bench_function(BenchmarkId::new("seq", task_count), |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            for _ in 0..task_count {
                black_box(work().await);
            }
        });
    })
    .bench_function(
        BenchmarkId::new("futures_concurrency::join", task_count),
        |b| {
            b.to_async(FuturesExecutor).iter(|| async {
                let mut futures = Vec::with_capacity(task_count);
                futures.resize_with(task_count, work);
                black_box(Join::join(futures).await);
            });
        },
    )
    .bench_function(
        BenchmarkId::new("futures_concurrency::FutureGroup", task_count),
        |b| {
            b.to_async(FuturesExecutor).iter(|| async {
                let mut group = pin!(iter::repeat_with(work)
                    .take(task_count)
                    .collect::<FutureGroup<_>>());

                while let Some(x) = group.next().await {
                    black_box(x);
                }
                black_box(&mut group);
            });
        },
    )
    .bench_function(
        BenchmarkId::new("async_executor::LocalExecutor", task_count),
        |b| {
            let ex = async_executor::LocalExecutor::new();
            b.to_async(FuturesExecutor).iter(|| {
                ex.run(async {
                    let mut tasks = Vec::with_capacity(task_count);
                    tasks.resize_with(task_count, || ex.spawn(work()));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                })
            })
        },
    )
    .bench_function(
        BenchmarkId::new("unsend::executor::Executor", task_count),
        |b| {
            let ex = unsend::executor::Executor::new();
            b.to_async(FuturesExecutor).iter(|| {
                ex.run(async {
                    let mut tasks = Vec::with_capacity(task_count);
                    tasks.resize_with(task_count, || ex.spawn(work()));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                })
            })
        },
    );
    for rt in [
        TokioRuntimeFlavor::CurrentThread,
        TokioRuntimeFlavor::MultiThread,
    ] {
        let param = TokioParams {
            rt,
            tasks: task_count,
        };
        c.bench_function(BenchmarkId::new("tokio::task::spawn_local", param), |b| {
            let rt = param.rt.builder().build().unwrap();
            b.to_async(rt).iter(|| async {
                let mut tasks = Vec::with_capacity(task_count);
                tasks.resize_with(task_count, || tokio::task::spawn_local(work()));

                for task in tasks.drain(..) {
                    black_box(task.await.unwrap());
                }
            })
        })
        .bench_function(
            BenchmarkId::new("tokio::task::JoinSet::spawn_local", param),
            |b| {
                let rt = param.rt.builder().build().unwrap();
                b.to_async(rt).iter(|| async {
                    let local_set = LocalSet::new();
                    let mut set = JoinSet::new();
                    for _ in 0..task_count {
                        set.spawn_local_on(work(), &local_set);
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
            },
        );
    }
}

fn shallow_many<F, T, M>(c: &mut BenchmarkGroup<'_, M>, work: F, task_count: usize)
where
    F: Fn() -> T + Copy + Send + Sync + 'static,
    T: Future + Send + Sync + 'static,
    T::Output: Send,
    M: Measurement,
{
    shallow_many_local(c, work, task_count);
    c.bench_function(
        BenchmarkId::new("async_executor::Executor", task_count),
        |b| {
            let ex = async_executor::Executor::new();
            b.to_async(FuturesExecutor).iter(|| {
                ex.run(async {
                    let mut tasks = Vec::with_capacity(task_count);
                    tasks.resize_with(task_count, || ex.spawn(work()));

                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                })
            })
        },
    );
    for rt in [
        TokioRuntimeFlavor::CurrentThread,
        TokioRuntimeFlavor::MultiThread,
    ] {
        let param = TokioParams {
            rt,
            tasks: task_count,
        };
        c.bench_function(BenchmarkId::new("tokio::task::spawn", param), |b| {
            let rt = param.rt.builder().build().unwrap();
            b.to_async(rt).iter(|| async {
                let mut tasks = Vec::with_capacity(task_count);
                tasks.resize_with(task_count, || tokio::task::spawn(work()));

                for task in tasks.drain(..) {
                    black_box(task.await.unwrap());
                }
            })
        })
        .bench_function(
            BenchmarkId::new("tokio::task::JoinSet::spawn", param),
            |b| {
                let rt = param.rt.builder().build().unwrap();
                b.to_async(rt).iter(|| async {
                    let mut set = iter::repeat_with(work)
                        .take(task_count)
                        .collect::<JoinSet<_>>();

                    while let Some(x) = set.join_next().await {
                        black_box(x.unwrap());
                    }
                    black_box(&mut set);
                })
            },
        );
    }
}
