use std::{
    fmt,
    future::Future,
    hash::{DefaultHasher, Hash, Hasher},
    iter, mem,
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
    // TODO: this number of tasks only gives 12% cache misses while 1_000_000 gives ~60% cache
    // misses, benchmark should cover this.
    let task_count: usize = std::env::var("TASK_COUNT").map_or(100_000, |s| s.parse().unwrap());
    let throughput = criterion::Throughput::Elements(task_count.try_into().unwrap());

    shallow_many(
        // TODO: tasks should be bench parameter not a group
        c.benchmark_group("ready_task")
            .throughput(throughput.clone()),
        || iter::repeat_with(|| async { black_box(1) }).take(task_count),
        false,
    );
    shallow_many(
        c.benchmark_group("yield_once_task")
            .throughput(throughput.clone()),
        || {
            iter::repeat_with(|| async {
                yield_now().await;
                black_box(1)
            })
            .take(task_count)
        },
        false,
    );
    shallow_many(
        c.benchmark_group("yield_ten_task")
            .throughput(throughput.clone()),
        || {
            iter::repeat_with(|| async {
                for _ in 0..10 {
                    yield_now().await;
                }
                black_box(1)
            })
            .take(task_count)
        },
        false,
    );
    shallow_many(
        c.benchmark_group("yield_hundred_task")
            .throughput(throughput.clone()),
        || {
            iter::repeat_with(|| async {
                for _ in 0..100 {
                    yield_now().await;
                }
                black_box(1)
            })
            .take(task_count)
        },
        false,
    );
    shallow_many(
        c.benchmark_group("independent_yield_rand_uniform_tasks")
            .throughput(throughput.clone()),
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
            .take(task_count)
        },
        false,
    );
    shallow_many(
        c.benchmark_group("fully_interdependent_tasks")
            .throughput(throughput.clone()),
        || interdependent_tasks(task_count, task_count),
        true,
    );
    shallow_many(
        c.benchmark_group("sqrt_interdependent_tasks")
            .throughput(throughput.clone()),
        || interdependent_tasks(task_count, (task_count as f64).sqrt() as usize),
        true,
    );
    shallow_many(
        c.benchmark_group("100000_interdependent_tasks")
            .throughput(throughput.clone()),
        || interdependent_tasks(task_count, 100000),
        true,
    );
    shallow_many(
        c.benchmark_group("10000_interdependent_tasks")
            .throughput(throughput.clone()),
        || interdependent_tasks(task_count, 10000),
        true,
    );
    shallow_many(
        c.benchmark_group("1000_interdependent_tasks")
            .throughput(throughput.clone()),
        || interdependent_tasks(task_count, 1000),
        true,
    );
    shallow_many(
        c.benchmark_group("100_interdependent_tasks")
            .throughput(throughput.clone()),
        || interdependent_tasks(task_count, 100),
        true,
    );
    shallow_many(
        c.benchmark_group("10_interdependent_tasks")
            .throughput(throughput.clone()),
        || interdependent_tasks(task_count, 10),
        true,
    );
}

/// Make large amount of interdependent tasks, this is structurally similair to actors
fn interdependent_tasks(task_count: usize, batch_count: usize) -> impl Tasks {
    let mut rng = rng_from_pkg_name();
    let (mut left, mut right) = iter::repeat_with(|| {
        let [left, right] = futures_concurrency_benchmark::handshake();
        (Some(left), Some(right))
    })
    .take(task_count)
    .collect::<(Vec<_>, Vec<_>)>();

    // Break handshake cycle
    for i in (0..task_count).step_by(batch_count) {
        left[i] = None;
        right[i] = None;
    }

    // Create a cycle of channels to cover every task and send a signal from somewhere
    let cycle = rand::seq::index::sample(&mut rng, task_count, task_count);
    let mut buf = left[cycle.index(task_count - 1)].take();
    for next in cycle {
        mem::swap(&mut left[next], &mut buf);
    }
    assert!(buf.is_none());

    left.into_iter().zip(right).map(|(left, right)| async {
        if let Some(left) = left {
            left.await;
        }
        if let Some(right) = right {
            right.await;
        }
    })
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

fn shallow_many_local<F, M>(c: &mut BenchmarkGroup<'_, M>, work: F, requires_concurrency: bool)
where
    F: LocalTaskGenerator + Copy,
    M: Measurement,
{
    if !requires_concurrency {
        c.bench_function("seq", |b| {
            // TODO: make and try FuturesLiteExecutor
            b.to_async(FuturesExecutor).iter(|| async {
                for task in work.iter_local_tasks() {
                    black_box(task.await);
                }
            });
        });
    }
    c.bench_function("alt_join::Join", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            let mut join = pin!(alt_join::Join::from_iterable(
                work.iter_local_tasks().collect::<Vec<_>>(),
            ));
            join.as_mut().await;
            black_box(join);
        });
    })
    .bench_function("futures_concurrency::join", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            black_box(
                futures_concurrency::future::Join::join(
                    work.iter_local_tasks().collect::<Vec<_>>(),
                )
                .await,
            );
        });
    })
    .bench_function("futures_concurrency::FutureGroup", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            let mut group = pin!(work
                .iter_local_tasks()
                .collect::<futures_concurrency::future::FutureGroup<_>>());

            while let Some(x) = group.next().await {
                black_box(x);
            }
        });
    })
    .bench_function("futures_util::future::JoinAll", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            black_box(
                work.iter_local_tasks()
                    .collect::<futures_util::future::JoinAll<_>>()
                    .await,
            );
        });
    })
    .bench_function("futures_util::stream::FuturesOrdered", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            let mut group = pin!(work
                .iter_local_tasks()
                .collect::<futures_util::stream::FuturesOrdered<_>>());

            while let Some(x) = group.next().await {
                black_box(x);
            }
        });
    })
    .bench_function("futures_util::stream::FuturesUnordered", |b| {
        b.to_async(FuturesExecutor).iter(|| async {
            let mut group = pin!(work
                .iter_local_tasks()
                .collect::<futures_util::stream::FuturesUnordered<_>>());

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
                let tasks = work
                    .iter_local_tasks()
                    .map(|t| ex.spawn(t))
                    .collect::<Vec<_>>();
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
                let tasks = work
                    .iter_local_tasks()
                    .map(|t| ex.spawn(t))
                    .collect::<Vec<_>>();
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
                let tasks = work
                    .iter_local_tasks()
                    .map(|t| local_set.spawn_local(t))
                    .collect::<Vec<_>>();

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
                for task in work.iter_local_tasks() {
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

fn shallow_many<F, M>(c: &mut BenchmarkGroup<'_, M>, work: F, requires_concurrency: bool)
where
    F: TaskGenerator + Copy,
    M: Measurement,
{
    shallow_many_local(c, work, requires_concurrency);
    c.bench_function("async_executor::Executor", |b| {
        let ex = async_executor::Executor::new();
        b.to_async(FuturesExecutor).iter(|| {
            ex.run(async {
                let mut tasks = Vec::new();
                ex.spawn_many(work.iter_tasks(), &mut tasks);

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
                let tasks = work
                    .iter_tasks()
                    .map(tokio::task::spawn)
                    .collect::<Vec<_>>();
                for task in tasks {
                    black_box(task.await.unwrap());
                }
            })
        })
        .bench_function(format!("tokio::task::JoinSet::spawn/{rt}"), |b| {
            let rt = rt.tokio_runtime_builder().build().unwrap();
            b.to_async(rt).iter(|| async {
                let mut set = work.iter_tasks().collect::<JoinSet<_>>();

                while let Some(x) = set.join_next().await {
                    black_box(x.unwrap());
                }
                black_box(&mut set);
            })
        });
    }
}

trait LocalTasks {
    type LocalTask: Future<Output = Self::LocalOutput> + 'static;
    type LocalOutput: 'static;
    fn next_local_task(&mut self) -> Option<Self::LocalTask>;

    #[inline(always)]
    fn into_iter_local(self) -> LocalTasksIter<Self>
    where
        Self: Sized,
    {
        LocalTasksIter(self)
    }
}

impl<I, T, O> LocalTasks for I
where
    I: Iterator<Item = T>,
    T: Future<Output = O> + 'static,
    O: 'static,
{
    type LocalTask = T;
    type LocalOutput = O;

    #[inline(always)]
    fn next_local_task(&mut self) -> Option<Self::LocalTask> {
        self.next()
    }
}

trait Tasks: LocalTasks {
    type Task: Future<Output = Self::Output> + Send + 'static;
    type Output: Send + 'static;
    fn next_task(&mut self) -> Option<Self::Task>;

    #[inline(always)]
    fn into_iter(self) -> TasksIter<Self>
    where
        Self: Sized,
    {
        TasksIter(self)
    }
}

impl<I, T, O> Tasks for I
where
    I: Iterator<Item = T>,
    T: Future<Output = O> + Send + 'static,
    O: Send + 'static,
{
    type Task = T;
    type Output = O;

    #[inline(always)]
    fn next_task(&mut self) -> Option<Self::Task> {
        self.next()
    }
}

trait LocalTaskGenerator {
    type LocalTasks: LocalTasks;
    fn generate_local_tasks(&self) -> Self::LocalTasks;

    #[inline(always)]
    fn iter_local_tasks(&self) -> LocalTasksIter<Self::LocalTasks> {
        self.generate_local_tasks().into_iter_local()
    }
}

impl<F, T> LocalTaskGenerator for F
where
    F: Fn() -> T,
    T: LocalTasks,
{
    type LocalTasks = T;

    #[inline(always)]
    fn generate_local_tasks(&self) -> Self::LocalTasks {
        self()
    }
}

trait TaskGenerator: LocalTaskGenerator {
    type Tasks: Tasks;
    fn generate_tasks(&self) -> Self::Tasks;

    #[inline(always)]
    fn iter_tasks(&self) -> TasksIter<Self::Tasks> {
        self.generate_tasks().into_iter()
    }
}

impl<F, T> TaskGenerator for F
where
    F: Fn() -> T,
    T: Tasks,
{
    type Tasks = T;

    #[inline(always)]
    fn generate_tasks(&self) -> Self::Tasks {
        self()
    }
}

struct LocalTasksIter<T>(T);

impl<T: LocalTasks> Iterator for LocalTasksIter<T> {
    type Item = T::LocalTask;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next_local_task()
    }
}

struct TasksIter<T>(T);

impl<T: Tasks> Iterator for TasksIter<T> {
    type Item = T::Task;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next_task()
    }
}
