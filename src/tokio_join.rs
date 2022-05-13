//! Join two values, one that implement `AsyncRead` and another that
//! implements `AsyncWrite` into one handle that implements both.
//!
//! Can be useful for stitching together stdin and stdout of a process
//! that can then be handed to a framed codec for example.

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use std::cell::UnsafeCell;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Release};
use std::sync::Arc;
use std::task::{Context, Poll};

macro_rules! ready {
    ($e:expr $(,)?) => {
        match $e {
            std::task::Poll::Ready(t) => t,
            std::task::Poll::Pending => return std::task::Poll::Pending,
        }
    };
}

/// Contains the reader and writer, returns by the [`join`](join()) method.
pub struct JointReaderWriter<R, W> {
    inner: Arc<Inner<R, W>>,
}

/// Join two values, one that implement `AsyncRead` and another that
/// implements `AsyncWrite` into one handle that implements both.
///
/// To split them back out use [`unjoin`](JointReaderWriter::unjoin()).
pub fn join<R, W>(reader: R, writer: W) -> JointReaderWriter<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    let inner = Arc::new(Inner {
        locked: AtomicBool::new(false),
        reader: UnsafeCell::new(reader),
        writer: UnsafeCell::new(writer),
    });

    JointReaderWriter { inner }
}

impl<R: AsyncRead, W> AsyncRead for JointReaderWriter<R, W> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut inner = ready!(self.inner.poll_lock(cx));
        inner.reader_pin().poll_read(cx, buf)
    }
}

impl<R, W: AsyncWrite> AsyncWrite for JointReaderWriter<R, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let mut inner = ready!(self.inner.poll_lock(cx));
        inner.writer_pin().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut inner = ready!(self.inner.poll_lock(cx));
        inner.writer_pin().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut inner = ready!(self.inner.poll_lock(cx));
        inner.writer_pin().poll_shutdown(cx)
    }
}

struct Inner<R, W> {
    locked: AtomicBool,
    reader: UnsafeCell<R>,
    writer: UnsafeCell<W>,
}

struct Guard<'a, R, W> {
    inner: &'a Inner<R, W>,
}

impl<R, W> Inner<R, W> {
    fn poll_lock(&self, cx: &mut Context<'_>) -> Poll<Guard<'_, R, W>> {
        if self
            .locked
            .compare_exchange(false, true, Acquire, Acquire)
            .is_ok()
        {
            Poll::Ready(Guard { inner: self })
        } else {
            // Spin... but investigate a better strategy

            std::thread::yield_now();
            cx.waker().wake_by_ref();

            Poll::Pending
        }
    }
}

impl<R, W> Guard<'_, R, W> {
    fn reader_pin(&mut self) -> Pin<&mut R> {
        // safety: TODO the stream is pinned in `Arc` and the `Guard` ensures mutual
        // exclusion.
        unsafe { Pin::new_unchecked(&mut *self.inner.reader.get()) }
    }

    fn writer_pin(&mut self) -> Pin<&mut W> {
        // safety: TODO the stream is pinned in `Arc` and the `Guard` ensures mutual
        // exclusion.
        unsafe { Pin::new_unchecked(&mut *self.inner.writer.get()) }
    }
}

impl<R, W> Drop for Guard<'_, R, W> {
    fn drop(&mut self) {
        self.inner.locked.store(false, Release);
    }
}

unsafe impl<R, W: Send> Send for JointReaderWriter<R, W> {}
unsafe impl<R, W: Sync> Sync for JointReaderWriter<R, W> {}

impl<R, W: fmt::Debug> fmt::Debug for JointReaderWriter<R, W> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("join::JointReaderWriter").finish()
    }
}
