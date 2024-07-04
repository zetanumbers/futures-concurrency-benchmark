use std::{future, task::Poll};

#[test]
fn simple() {
    let sides = futures_concurrency_benchmark::handshake();
    futures_lite::future::block_on(future::poll_fn(|cx| {
        if let [Poll::Ready(()), Poll::Ready(())] = sides.each_ref().map(|side| side.ready(cx)) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
}
