#![deny(
    deprecated,
    rust_2024_compatibility,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
#![allow(
    // Not my style.
    clippy::use_self,
    // API may still change.
    clippy::missing_const_for_fn,

    // TODO: fix
    clippy::future_not_send,
)]

use core::{
    future::poll_fn,
    pin::Pin,
    task::{ready, Context, Poll},
};
use std::io::{self, IoSlice, IoSliceMut};

use futures_io::{AsyncRead, AsyncWrite};

pub struct Buffer {
    /// Absolute counter of bytes read from the buffer whose modulus is the
    /// starting offset of readable data. May get reset to 0 when it catches up
    /// with [`write`](Buffer::write).
    read: usize,
    /// Absolute counter of bytes written into the buffer whose modulus is the
    /// end offset of readable data and the offset where new data is written to.
    /// May get reset to 0 when [`read`](Buffer::read) catches up.
    write: usize,
    /// Bitmask derived from the power-of-2 capacity to quickly calculate the
    /// [`read`](Buffer::read) and [`write`](Buffer::write) offsets.
    mask: usize,
    /// Heap-allocated memory for data.
    // TODO: page (start) aligned, if possible without unsafe.
    data: Box<[u8]>,
}

impl Buffer {
    /// Creates a new fixed-capacity [`Buffer`].
    ///
    /// `capacity` must be a power of 2.
    /// (This requirement may get dropped in a future version.)
    ///
    /// # Panics
    ///
    /// Will panic if [`capacity`] is not a power of 2.
    #[must_use]
    #[inline]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity is not power of two: {capacity}"
        );
        Buffer {
            read: 0,
            write: 0,
            mask: capacity - 1,
            data: vec![0u8; capacity].into_boxed_slice(),
        }
    }

    /// Clears the buffer of data.
    ///
    /// Does not zero the internal buffer.
    #[inline]
    pub fn clear(&mut self) {
        self.read = 0;
        self.write = 0;
    }

    /// Returns the amount of data in the buffer.
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.write - self.read
    }

    /// Returns `true` if the buffer contains no data.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.read == self.write
    }

    /// The offset of the read position into the internal buffer.
    #[must_use]
    #[inline]
    const fn read_offset(&self) -> usize {
        self.read & self.mask
    }

    /// The offset of the write position into the internal buffer.
    #[must_use]
    #[inline]
    const fn write_offset(&self) -> usize {
        self.write & self.mask
    }

    /// Returns a pair of byte slices containing the filled data of the buffer.
    ///
    /// The second slice or both slices may have a lnegth of 0 when the buffer
    /// is completely empty.
    /// After reading [`read_advance`] must be called with the number of bytes
    /// read.
    #[must_use]
    #[inline]
    pub fn read_slices(&self) -> [&[u8]; 2] {
        let (read, write) = (self.read_offset(), self.write_offset());

        if read <= write {
            [&self.data[read..write], &[]]
        } else {
            let (b, a) = self.data.split_at(read);
            [a, &b[..write]]
        }
    }

    /// Returns a pair of [`IoSlices`](IoSlice) for reading from this buffer.
    ///
    /// The second slice or both slices may have a lnegth of 0 when the buffer
    /// is completely empty.
    /// After reading [`read_advance`] must be called with the number of bytes
    /// read.
    #[must_use]
    #[inline]
    pub fn read_io_slices(&self) -> [IoSlice<'_>; 2] {
        let [a, b] = self.read_slices();
        [IoSlice::new(a), IoSlice::new(b)]
    }

    /// Advances the internal read position.
    ///
    /// Must be called after reading from the slices returned by [`read_slices`]
    /// and [`read_io_slices`].
    ///
    /// # Panics
    ///
    /// Will panic if [`n`] is higher than number of bytes in buffer.
    #[inline]
    pub fn read_advance(&mut self, n: usize) {
        let new_read = self.read + n;
        assert!(new_read <= self.write, "read advance out of bounds");
        if new_read == self.write {
            // TODO: consider not resetting counters and expose both for stats.
            self.clear();
        } else {
            self.read = new_read;
        }
    }

    /// Returns a pair of byte slices containing the empty space of the buffer.
    ///
    /// The second slice has a length of 0 when the entire empty segment of
    /// the buffer can be mapped by the first slice.
    /// Both slices have a length of 0 when the buffer
    /// is filled to capacity.
    /// After writing [`write_advance`] must be called with the number of bytes
    /// written.
    ///
    /// The slices should only be written to and not read from because they
    /// contain old data. If the slices are passed to an untrusted data source
    /// they should be zeroed first.
    #[must_use]
    #[inline]
    pub fn write_slices(&mut self) -> [&mut [u8]; 2] {
        let (read, write) = (self.read_offset(), self.write_offset());

        if read > write {
            [&mut self.data[write..read], &mut []]
        } else {
            let (b, a) = self.data.split_at_mut(write);
            [a, &mut b[..read]]
        }
    }

    /// Returns a pair of [`IoSliceMuts`](IoSliceMut) containing the empty space
    /// of the buffer.
    ///
    /// The second slice has a length of 0 when the entire empty segment of
    /// the buffer can be mapped by the first slice.
    /// Both slices have a length of 0 when the buffer
    /// is filled to capacity.
    /// After writing [`write_advance`] must be called with the number of bytes
    /// written.
    ///
    /// The slices should only be written to and not read from because they
    /// contain old data. If the slices are passed to an untrusted data source
    /// they should be zeroed first.
    #[must_use]
    #[inline]
    pub fn write_io_slices(&mut self) -> [IoSliceMut<'_>; 2] {
        let [a, b] = self.write_slices();
        [IoSliceMut::new(a), IoSliceMut::new(b)]
    }

    /// Advances the internal write position.
    ///
    /// Must be called after writing to the slices returned by [`write_slices`]
    /// and [`write_io_slices`].
    ///
    /// # Panics
    ///
    /// Will panic if [`n`] is higher than empty capacity in buffer.
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

    /// Attempts to read from the [`AsyncRead`].
    ///
    /// Returns [`Poll::Pending`] if nothing can be read at the moment.
    /// Returns [`Poll::Ready`] with [`Ok`] and the
    /// number of bytes read on success.
    /// (The internal read position is advanced implicitly on successful read.)
    ///
    /// # Errors
    ///
    /// On error, [`Err`] returns the I/O error from the [`AsyncRead`].
    #[inline]
    pub fn poll_read_from<R: AsyncRead + ?Sized>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read: Pin<&mut R>,
    ) -> Poll<io::Result<usize>> {
        let n = ready!(read.poll_read_vectored(cx, &mut self.write_io_slices()))?;
        self.write_advance(n);
        Poll::Ready(Ok(n))
    }

    /// Reads from the [`AsyncRead`].
    ///
    /// Returns [`Ok`] and the number of bytes read on success.
    /// (The internal read position is advanced implicitly.)
    ///
    /// # Errors
    ///
    /// On error, [`Err`] returns the I/O error from the [`AsyncRead`].
    #[inline]
    pub async fn read_from<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        mut read: &mut R,
    ) -> io::Result<usize> {
        let mut this = Pin::new(self);
        poll_fn(|cx| this.as_mut().poll_read_from(cx, Pin::new(&mut read))).await
    }

    /// Attempts to write to the [`AsyncWrite`].
    ///
    /// Returns [`Poll::Pending`] if nothing can be written at the moment.
    /// Returns [`Poll::Ready`] with [`Ok`] and the
    /// number of bytes written on success.
    /// (The internal write position is advanced implicitly on successful write.)
    ///
    /// # Errors
    ///
    /// On error, [`Err`] returns the I/O error from the [`AsyncWrite`].
    #[inline]
    pub fn poll_write_to<W: AsyncWrite + ?Sized>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        write: Pin<&mut W>,
    ) -> Poll<io::Result<usize>> {
        let n = ready!(write.poll_write_vectored(cx, &self.read_io_slices()))?;
        self.read_advance(n);
        Poll::Ready(Ok(n))
    }

    /// Writes to the [`AsyncWrite`].
    ///
    /// Returns [`Ok`] and the number of bytes written on success.
    /// (The internal write position is advanced implicitly.)
    ///
    /// # Errors
    ///
    /// On error, [`Err`] returns the I/O error from the [`AsyncWrite`].
    #[inline]
    pub async fn write_to<W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        mut write: &mut W,
    ) -> io::Result<usize> {
        let mut this = Pin::new(self);
        poll_fn(|cx| this.as_mut().poll_write_to(cx, Pin::new(&mut write))).await
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
