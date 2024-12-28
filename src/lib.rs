use core::{
    future::poll_fn,
    pin::Pin,
    task::{ready, Context, Poll},
};
use std::io::{self, IoSlice, IoSliceMut};

use futures_io::{AsyncRead, AsyncWrite};

pub struct Buffer {
    read: usize,
    write: usize,
    mask: usize,
    data: Box<[u8]>,
}

impl Buffer {
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity is not power of two: {capacity}"
        );
        Buffer {
            read: 0,
            write: 0,
            mask: capacity - 1,
            data: vec![0u8; capacity].into(),
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.read = 0;
        self.write = 0;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.write - self.read
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.read == self.write
    }

    #[inline]
    fn read_offset(&self) -> usize {
        self.read & self.mask
    }

    #[inline]
    fn write_offset(&self) -> usize {
        self.write & self.mask
    }

    #[inline]
    pub fn read_slices(&self) -> [&[u8]; 2] {
        let (read, write) = (self.read_offset(), self.write_offset());

        if read < write {
            [&self.data[read..write], &[]]
        } else if read > write {
            let (b, a) = self.data.split_at(read);
            [a, &b[..write]]
        } else {
            [&[], &[]]
        }
    }

    #[inline]
    pub fn read_io_slices(&self) -> [IoSlice<'_>; 2] {
        let [a, b] = self.read_slices();
        [IoSlice::new(a), IoSlice::new(b)]
    }

    #[inline]
    pub fn read_advance(&mut self, n: usize) {
        let new_read = self.read + n;
        assert!(new_read <= self.write, "read advance out of bounds");
        if new_read == self.write {
            self.clear();
        } else {
            self.read = new_read;
        }
    }

    #[inline]
    pub fn write_slices(&mut self) -> [&mut [u8]; 2] {
        let (read, write) = (self.read_offset(), self.write_offset());

        if read < write {
            let (b, a) = self.data.split_at_mut(write);
            [a, &mut b[..read]]
        } else if read > write {
            [&mut self.data[write..read], &mut []]
        } else {
            [&mut self.data, &mut []]
        }
    }

    #[inline]
    pub fn write_io_slices(&mut self) -> [IoSliceMut<'_>; 2] {
        let [a, b] = self.write_slices();
        [IoSliceMut::new(a), IoSliceMut::new(b)]
    }

    #[inline]
    pub fn write_advance(&mut self, n: usize) {
        debug_assert!(self.read <= self.write);
        let new_write = self.write + n;
        assert!(
            new_write - self.read <= self.data.len(),
            "write advance out of bounds"
        );
        self.write = new_write;
    }

    #[inline]
    pub fn poll_read_from<R: AsyncRead>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read: Pin<&mut R>,
    ) -> Poll<io::Result<()>> {
        let n = ready!(AsyncRead::poll_read_vectored(
            read,
            cx,
            &mut self.write_io_slices()
        ))?;
        self.write_advance(n);
        Poll::Ready(Ok(()))
    }

    #[inline]
    pub async fn read_from<R: AsyncRead + Unpin>(&mut self, read: &mut R) -> io::Result<()> {
        let mut this = Pin::new(self);
        poll_fn(|cx| this.as_mut().poll_read_from(cx, Pin::new(read))).await
    }

    #[inline]
    pub fn poll_write_to<W: AsyncWrite>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        write: Pin<&mut W>,
    ) -> Poll<io::Result<()>> {
        let n = ready!(AsyncWrite::poll_write_vectored(
            write,
            cx,
            &self.read_io_slices()
        ))?;
        self.read_advance(n);
        Poll::Ready(Ok(()))
    }

    #[inline]
    pub async fn write_to<W: AsyncWrite + Unpin>(&mut self, write: &mut W) -> io::Result<()> {
        let mut this = Pin::new(self);
        poll_fn(|cx| this.as_mut().poll_write_to(cx, Pin::new(write))).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_advance() {
        let mut buf = Buffer::new(16);
        // 0123456789012345
        // ----------------
        assert_eq!(0, buf.len());

        let [a, b] = buf.write_io_slices();
        assert_eq!(16, a.len());
        assert_eq!(0, b.len());
        let [a, b] = buf.read_io_slices();
        assert_eq!(0, a.len());
        assert_eq!(0, b.len());

        buf.write_advance(3);
        // 0123456789012345
        // ***-------------
        assert_eq!(3, buf.len());

        let [a, b] = buf.write_io_slices();
        assert_eq!(13, a.len());
        assert_eq!(0, b.len());
        let [a, b] = buf.read_io_slices();
        assert_eq!(3, a.len());
        assert_eq!(0, b.len());

        buf.write_advance(7);
        // 0123456789012345
        // **********------
        assert_eq!(10, buf.len());

        let [a, b] = buf.write_io_slices();
        assert_eq!(6, a.len());
        assert_eq!(0, b.len());
        let [a, b] = buf.read_io_slices();
        assert_eq!(10, a.len());
        assert_eq!(0, b.len());

        buf.read_advance(8);
        // 0123456789012345
        // --------**------
        assert_eq!(2, buf.len());

        let [a, b] = buf.write_io_slices();
        assert_eq!(6, a.len());
        assert_eq!(8, b.len());
        let [a, b] = buf.read_io_slices();
        assert_eq!(2, a.len());
        assert_eq!(0, b.len());

        buf.write_advance(9);
        // 0123456789012345
        // ***-----********
        assert_eq!(11, buf.len());

        let [a, b] = buf.write_io_slices();
        assert_eq!(5, a.len());
        assert_eq!(0, b.len());
        let [a, b] = buf.read_io_slices();
        assert_eq!(8, a.len());
        assert_eq!(3, b.len());

        buf.read_advance(10);
        // 0123456789012345
        // --*-------------
        assert_eq!(1, buf.len());

        let [a, b] = buf.write_io_slices();
        assert_eq!(13, a.len());
        assert_eq!(2, b.len());
        let [a, b] = buf.read_io_slices();
        assert_eq!(1, a.len());
        assert_eq!(0, b.len());

        buf.read_advance(1);
        // 0123456789012345
        // ----------------
        assert_eq!(0, buf.len());

        let [a, b] = buf.write_io_slices();
        assert_eq!(16, a.len());
        assert_eq!(0, b.len());
        let [a, b] = buf.read_io_slices();
        assert_eq!(0, a.len());
        assert_eq!(0, b.len());
    }
}
