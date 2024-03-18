use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures_concurrency::prelude::*;
use futures_lite::{
    future::{self, block_on},
    prelude::*,
};

fn join(c: &mut Criterion) {
    c.benchmark_group("join")
        .sample_size(10)
        .bench_function("futures_concurrency::join", |b| {
            let readies = || vec![future::ready(1); 1_000_000];

            b.iter(|| {
                let readies = readies();
                black_box(block_on(readies.join()));
            })
        });
}

fn group(c: &mut Criterion) {
    let mut group = futures_concurrency::future::FutureGroup::with_capacity(1_000_000);

    c.benchmark_group("group").sample_size(10).bench_function(
        "futures_concurrency::FutureGroup",
        |b| {
            b.iter(|| {
                for _ in 0..1_000_000 {
                    group.insert(future::ready(1));
                }

                block_on(async {
                    while let Some(x) = group.next().await {
                        black_box(x);
                    }
                });
            });
        },
    );
}

fn executor(c: &mut Criterion) {
    let ex = async_executor::LocalExecutor::new();
    let mut tasks = Vec::with_capacity(1_000_000);

    c.benchmark_group("executor")
        .sample_size(10)
        .bench_function("async_executor::LocalExecutor", |b| {
            b.iter(|| {
                for _ in 0..1_000_000 {
                    tasks.push(ex.spawn(future::ready(1)));
                }

                block_on(ex.run(async {
                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        });
}

fn unsend_executor(c: &mut Criterion) {
    let ex = unsend::executor::Executor::new();
    let mut tasks = Vec::with_capacity(1_000_000);

    c.benchmark_group("executor")
        .sample_size(10)
        .bench_function("async_executor::LocalExecutor", |b| {
            b.iter(|| {
                for _ in 0..1_000_000 {
                    tasks.push(ex.spawn(future::ready(1)));
                }

                block_on(ex.run(async {
                    for task in tasks.drain(..) {
                        black_box(task.await);
                    }
                }));
            })
        });
}

criterion_group!(benches, join, group, executor, unsend_executor);

criterion_main!(benches);
