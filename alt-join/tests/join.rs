use std::{future::Future, hint, iter, pin::pin, task};

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
        let mut join = pin!(alt_join::Join::from_vec(
            iter::repeat_with(|| future::ready(42)).take(i).collect(),
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
        let mut join = pin!(alt_join::Join::from_vec(
            iter::repeat_with(|| future::yield_now()).take(i).collect(),
        ));
        assert_eq!(join.as_mut().poll(&mut cx), task::Poll::Pending);
        assert_eq!(join.as_mut().poll(&mut cx), task::Poll::Ready(()));
        hint::black_box(join);
    }
}
