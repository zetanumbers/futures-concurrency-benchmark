#![allow(unexpected_cfgs)]

// TODO: Steal some tests

use std::{future::Future, hint, pin::pin, task};

use noop_waker::noop_waker;

mod future {
    pub use futures_lite::future::yield_now;
    pub use std::future::ready;
}

#[test]
fn ready() {
    let waker = noop_waker();
    let mut cx = task::Context::from_waker(&waker);

    for i in 0..16 {
        let mut join = pin!(alt_join::Join::from_iterable(
            (0..i).map(|i| future::ready(i)),
        ));
        assert_eq!(join.as_mut().poll(&mut cx), task::Poll::Ready(()));
        hint::black_box(join);
    }
}

#[test]
fn yield_now() {
    let waker = noop_waker();
    let mut cx = task::Context::from_waker(&waker);

    for i in 1..16 {
        let mut join = pin!(alt_join::Join::from_iterable(
            (0..i).map(|_| future::yield_now()),
        ));
        assert_eq!(join.as_mut().poll(&mut cx), task::Poll::Pending);
        assert_eq!(join.as_mut().poll(&mut cx), task::Poll::Ready(()));
        hint::black_box(join);
    }
}

// TODO: isolate into a separate file
#[cfg(alt_join_loom)]
#[test]
fn loom_yield_now() {
    let f = || {
        let waker = noop_waker();
        let mut cx = task::Context::from_waker(&waker);

        for i in 1..=3 {
            let mut join = pin!(alt_join::Join::from_iterable(
                (0..i).map(|_| future::yield_now()),
            ));
            assert_eq!(join.as_mut().poll(&mut cx), task::Poll::Pending);
            assert_eq!(join.as_mut().poll(&mut cx), task::Poll::Ready(()));
            hint::black_box(join);
        }
    };

    loom::model(f)
}
