#![deny(
    clippy::undocumented_unsafe_blocks,
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
)]
#![cfg_attr(not(feature = "std"), no_std)]
#![no_implicit_prelude]

extern crate alloc;

use ::alloc::alloc::{alloc, dealloc, Layout};
use ::alloc::sync::Arc;
use ::core::cell::UnsafeCell;
use ::core::clone::Clone;
use ::core::convert::{AsMut, AsRef};
use ::core::marker::{PhantomData, Send, Sync};
use ::core::ops::{Deref, DerefMut, Drop, FnMut};
use ::core::ptr::NonNull;
use ::core::result::Result::{self, Ok};
use ::core::sync::atomic::AtomicUsize;
use ::core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use ::core::{assert, assert_eq, assert_ne, debug_assert, debug_assert_eq, slice};
#[cfg(feature = "std")]
use ::std::io;

#[derive(Debug)]
pub struct Buffer {
    inner: Arc<BufferInner>,
}

impl Buffer {
    /// Creates a new instance of specified `size` (capacity) and alignment.
    ///
    /// # Panics
    ///
    /// Will panic if `size` or `align` is not a power of 2.
    #[must_use]
    #[inline]
    pub fn new(size: usize, align: usize) -> Self {
        assert!(
            size.is_power_of_two(), // implies != 0
            "size is not power of two: {size}"
        );
        // TODO: consider accepting any modulus and let compiler optimize
        //       this into and-ing for power-of-2s.
        let mask = size - 1;

        let data = UnsafeCell::new(AlignedData::new(size, align));

        Buffer {
            inner: Arc::new(BufferInner {
                read: AtomicUsize::new(0),
                write: AtomicUsize::new(0),
                mask,
                data,
            }),
        }
    }

    /// Consumes the buffer and splits it into a `Reader` and a `Writer` half
    /// that can be send to other threads.
    #[must_use]
    #[inline]
    pub fn into_parts(self) -> (Reader, Writer) {
        let reader = Reader {
            buffer: Arc::clone(&self.inner),
            _notsync: PhantomData,
        };
        let writer = Writer {
            buffer: self.inner,
            _notsync: PhantomData,
        };
        (reader, writer)
    }
}

// TODO: put data and counters into same heap allocation.
#[derive(Debug)]
struct BufferInner {
    read: AtomicUsize,
    write: AtomicUsize,
    mask: usize,
    data: UnsafeCell<AlignedData>,
}

// SAFETY: Sync is safe because slices of data are guaranteed to not be aliased.
// TODO: make data sync, if possible. And switch to `SyncUnsafeCell`.
unsafe impl Sync for BufferInner {}

impl BufferInner {
    // SAFETY: callers of `data` and `data_mut` must make sure that the
    //         returned slice is split into non-overlapping subslices.
    // TODO: consider moving the segmentation here to make these accessors safe.
    #[must_use]
    #[inline]
    unsafe fn data(&self) -> &[u8] {
        // SAFETY: the UnsafeCell contains a properly initialized instance of
        //         AlignedData and the mut pointer can be cast to a non-mut ref.
        unsafe { &*(self.data.get()) }
    }

    // SAFETY: callers of `data` and `data_mut` must make sure that the
    //         returned slice is split into non-overlapping subslices.
    // TODO: consider moving the segmentation here to make these accessors safe.
    #[expect(clippy::mut_from_ref)]
    #[must_use]
    #[inline]
    unsafe fn data_mut(&self) -> &mut [u8] {
        // SAFETY: the UnsafeCell contains a properly initialized instance of
        //         AlignedData and the mut pointer can be cast to a mut ref.
        unsafe { &mut *self.data.get() }
    }

    #[inline]
    fn synced_read<E>(
        &self,
        mut f: impl FnMut([&mut [u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let r = self.read.load(Relaxed);
        let w = self.write.load(Acquire);

        // SAFETY: there can only be one reader and it further constrains the
        //         slice to the space between the raad and the write position.
        // TODO: consider moving the deref into reader to couple with safety
        //       guarantee.
        let data = unsafe { self.data_mut() };

        let (bufs, len) = empty_segments_for_write(data, self.mask, r, w);
        let n = f(bufs, len)?;
        debug_assert!(n <= len, "{n} <= {len}");

        self.write.store(w.checked_add(n).unwrap(), Release);
        Ok(n)
    }

    #[inline]
    fn synced_write<E>(
        &self,
        mut f: impl FnMut([&[u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let r = self.read.load(Relaxed);
        let w = self.write.load(Acquire);

        // SAFETY: there can only be one writer and it further constrains the
        //         slice to the space between the write and the read position.
        // TODO: consider moving the deref into writer to couple with safety
        //       guarantee.
        let data = unsafe { self.data() };

        let (bufs, len) = filled_segments_for_read(data, self.mask, r, w);
        let n = f(bufs, len)?;
        debug_assert!(n <= len, "{n} <= {len}");

        self.read.store(r.checked_add(n).unwrap(), Release);
        Ok(n)
    }
}

#[must_use]
#[inline]
fn filled_segments_for_read(
    data: &[u8],
    mask: usize,
    read: usize,
    write: usize,
) -> ([&[u8]; 2], usize) {
    debug_assert!(read <= write);
    debug_assert!(write <= read + data.len());

    let len = write - read;
    let start = read & mask;
    let end = start + len;
    let endw = end & mask;

    let bufs = if end == endw {
        [&data[start..end], &[]]
    } else {
        let (b, a) = data.split_at(start);
        [a, &b[..endw]]
    };

    debug_assert_eq!(bufs[0].len() + bufs[1].len(), len);
    (bufs, len)
}

#[must_use]
#[inline]
fn empty_segments_for_write(
    data: &mut [u8],
    mask: usize,
    read: usize,
    write: usize,
) -> ([&mut [u8]; 2], usize) {
    debug_assert!(read <= write);
    debug_assert!(write <= read + data.len());

    let len = data.len() - (write - read);
    let start = write & mask;
    let end = start + len;
    let endw = end & mask;

    let bufs = if end == endw {
        [&mut data[start..end], &mut []]
    } else {
        let (b, a) = data.split_at_mut(start);
        [a, &mut b[..endw]]
    };

    debug_assert_eq!(bufs[0].len() + bufs[1].len(), len);
    (bufs, len)
}

#[derive(Debug)]
pub struct Reader {
    buffer: Arc<BufferInner>,
    _notsync: PhantomData<*mut ()>,
}

// SAFETY: Reader is already Send, but to prevent Sync it contains a PhantomData
//         field preventing both traits. Until negative trait bounds are allowed
//         this Send impl is needed.
unsafe impl Send for Reader {}

impl Reader {
    /// Calls the passed closure with a pair of [`io::IoSliceMut`] meant to be
    /// used with [`io::Read::read_vectored`] and async variants and the total
    /// length of both slices.
    /// The closure must return the number of bytes read on success.
    ///
    /// # Errors
    ///
    /// Returns the error from the closure unchanged.
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_slices(
        &self,
        mut f: impl FnMut(&mut [io::IoSliceMut<'_>], usize) -> io::Result<usize>,
    ) -> io::Result<usize> {
        self.buffer.synced_read(|bufs, len| {
            let mut bufs = bufs.map(io::IoSliceMut::new);
            f(&mut bufs, len)
        })
    }

    /// Calls the passed closure with a pair of `&mut [u8]` meant to be
    /// used with non `std::io` vectored read operations.
    ///
    /// # Errors
    ///
    /// Returns the error from the closure unchanged.
    #[inline]
    pub fn slices<E>(
        &self,
        mut f: impl FnMut(&mut [&mut [u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        self.buffer.synced_read(|mut bufs, len| f(&mut bufs, len))
    }

    #[doc(hidden)]
    #[inline]
    pub fn position(&self) -> usize {
        self.buffer.write.load(Relaxed)
    }
}

#[derive(Debug)]
pub struct Writer {
    buffer: Arc<BufferInner>,
    _notsync: PhantomData<*mut ()>,
}

// SAFETY: Writer is already Send, but to prevent Sync it contains a PhantomData
//         field preventing both traits. Until negative trait bounds are allowed
//         this Send impl is needed.
unsafe impl Send for Writer {}

impl Writer {
    /// Calls the passed closure with a pair of [`io::IoSlice`] meant to be
    /// used with [`io::Read::write_vectored`] and async variants and the total
    /// length of both slices.
    /// The closure must return the number of bytes written on success.
    ///
    /// # Errors
    ///
    /// Returns the error from the closure unchanged.
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_slices(
        &self,
        mut f: impl FnMut(&[io::IoSlice<'_>], usize) -> io::Result<usize>,
    ) -> io::Result<usize> {
        self.buffer.synced_write(|bufs, len| {
            let bufs = bufs.map(io::IoSlice::new);
            f(&bufs, len)
        })
    }

    /// Calls the passed closure with a pair of `&[u8]` meant to be
    /// used with non `std::io` vectored write operations.
    ///
    /// # Errors
    ///
    /// Returns the error from the closure unchanged.
    #[inline]
    pub fn slices<E>(
        &self,
        mut f: impl FnMut(&[&[u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        self.buffer.synced_write(|bufs, len| f(&bufs, len))
    }

    #[doc(hidden)]
    #[inline]
    pub fn position(&self) -> usize {
        self.buffer.read.load(Relaxed)
    }

    #[doc(hidden)]
    #[inline]
    pub fn is_empty(&self) -> bool {
        let r = self.buffer.read.load(Relaxed);
        let w = self.buffer.write.load(Relaxed);
        w == r
    }
}

#[derive(Debug)]
struct AlignedData {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: Send is safe because pointer cannot be accessed directly.
//         Aliasing rules are followed by requiring a mut ref xor non-mut refs
//         to access the pointed to data.
unsafe impl Send for AlignedData {}

impl AlignedData {
    #[inline]
    fn new(size: usize, align: usize) -> Self {
        assert_ne!(size, 0, "size cannot be zero");

        let layout = Layout::from_size_align(size, align).unwrap();

        // SAFETY: alloc is called with a correct layout with a non-zero size.
        // A null pointer is immediately handled.
        let ptr = unsafe { NonNull::new(alloc(layout)).unwrap() };

        let addr = ptr.as_ptr() as usize;
        assert_eq!(addr % align, 0, "aligned alloc failed");

        AlignedData { ptr, layout }
    }

    #[must_use]
    #[inline]
    fn slice(&self) -> &[u8] {
        // SAFETY: from_raw_parts is called with the non-null pointer returned
        //         by alloc with the original size used by the layout.
        //         No alignment requirements due to u8.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }

    #[must_use]
    #[inline]
    fn slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: from_raw_parts_mut is called with the non-null pointer
        //         returned by alloc with the original size used by the layout.
        //         No alignment requirements due to u8.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedData {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: dealloc is called with the non-null pointer returned by
        //         alloc and the same layout.
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

impl AsRef<[u8]> for AlignedData {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.slice()
    }
}

impl AsMut<[u8]> for AlignedData {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.slice_mut()
    }
}

impl Deref for AlignedData {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.slice()
    }
}

impl DerefMut for AlignedData {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.slice_mut()
    }
}

#[cfg(test)]
mod tests {
    use ::core::marker::{Send, Sized, Sync};
    use ::static_assertions::{assert_impl_all, assert_not_impl_any};

    use super::*;

    assert_impl_all!(BufferInner: Send, Sync);
    assert_impl_all!(Reader: Send);
    assert_not_impl_any!(Reader: Sync);
    assert_impl_all!(Writer: Send);
    assert_not_impl_any!(Writer: Sync);

    #[test]
    fn test_filled_segments_for_read() {
        let data = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mask = 15;

        let (bufs, len) = filled_segments_for_read(&data, mask, 2, 13);
        assert_eq!(bufs[0], &[2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 11);

        let (bufs, len) = filled_segments_for_read(&data, mask, 17, 20);
        assert_eq!(bufs[0], &[1, 2, 3]);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 3);

        let (bufs, len) = filled_segments_for_read(&data, mask, 10, 20);
        assert_eq!(bufs[0], &[10, 11, 12, 13, 14, 15]);
        assert_eq!(bufs[1], &[0, 1, 2, 3]);
        assert_eq!(len, 10);

        let (bufs, len) = filled_segments_for_read(&data, mask, 16, 20);
        assert_eq!(bufs[0], &[0, 1, 2, 3]);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 4);

        let (bufs, len) = filled_segments_for_read(&data, mask, 0, 16);
        assert_eq!(
            bufs[0],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 16);

        let (bufs, len) = filled_segments_for_read(&data, mask, 0, 0);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);

        let (bufs, len) = filled_segments_for_read(&data, mask, 15, 15);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);

        let (bufs, len) = filled_segments_for_read(&data, mask, 16, 16);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);
    }

    #[test]
    fn test_empty_segments_for_write() {
        let mut data = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mask = 15;

        let (bufs, len) = empty_segments_for_write(&mut data, mask, 2, 13);
        assert_eq!(bufs[0], &[13, 14, 15]);
        assert_eq!(bufs[1], &[0, 1]);
        assert_eq!(len, 5);

        let (bufs, len) = empty_segments_for_write(&mut data, mask, 13, 17);
        assert_eq!(bufs[0], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(bufs[1], &[]);
        assert_eq!(len, 12);

        let (bufs, len) = empty_segments_for_write(&mut data, mask, 0, 16);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);

        let (bufs, len) = empty_segments_for_write(&mut data, mask, 0, 0);
        assert_eq!(
            bufs[0],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 16);

        let (bufs, len) = empty_segments_for_write(&mut data, mask, 15, 15);
        assert_eq!(bufs[0], &[15]);
        assert_eq!(bufs[1], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert_eq!(len, 16);

        let (bufs, len) = empty_segments_for_write(&mut data, mask, 16, 16);
        assert_eq!(
            bufs[0],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 16);
    }
}
