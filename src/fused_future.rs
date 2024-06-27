use std::{
    future::Future,
    pin::Pin,
    task::{self, Poll},
};

pub enum FusedFuture<F>
where
    F: Future,
{
    Pending(F),
    Ready(F::Output),
}

impl<F> FusedFuture<F>
where
    F: Future,
{
    pub const fn new(future: F) -> Self {
        FusedFuture::Pending(future)
    }
}

impl<F> FusedFuture<F>
where
    F: Future,
{
    pub fn into_output(self) -> Poll<F::Output> {
        match self {
            FusedFuture::Ready(v) => Poll::Ready(v),
            FusedFuture::Pending(_) => Poll::Pending,
        }
    }
}

impl<F> Future for FusedFuture<F>
where
    F: Future,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let out = unsafe {
            match self.as_mut().get_unchecked_mut() {
                FusedFuture::Pending(f) => task::ready!(Pin::new_unchecked(f).poll(cx)),
                FusedFuture::Ready(_) => return Poll::Ready(()),
            }
        };

        self.set(FusedFuture::Ready(out));
        Poll::Ready(())
    }
}
