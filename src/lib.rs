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

use ::alloc::alloc::{alloc_zeroed, dealloc, Layout};
use ::alloc::sync::Arc;
use ::core::clone::Clone;
use ::core::marker::{PhantomData, Send, Sync};
use ::core::ops::{Drop, FnMut, Range};
use ::core::ptr::{self, NonNull};
use ::core::result::Result::{self, Ok};
use ::core::sync::atomic::AtomicUsize;
use ::core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use ::core::{assert, assert_eq, assert_ne, debug_assert};
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

        let data = AlignedData::new(size, align);

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
    data: AlignedData,
}

// SAFETY: Sync is safe because slices of data are guaranteed to not be aliased.
// TODO: make data sync, if possible. And switch to `SyncUnsafeCell`.
unsafe impl Sync for BufferInner {}

impl BufferInner {
    #[inline]
    fn synced_read<E>(
        &self,
        mut f: impl FnMut([&mut [u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let w = self.write.load(Relaxed);
        let r = self.read.load(Acquire);

        let (ranges, len) = empty_ranges(self.data.len(), self.mask, r, w);
        // SAFETY: ranges are guaranteed to not overlap with any ranges
        //         `synced_write` will use at the same time.
        let bufs = unsafe { self.data.slices_mut(ranges) };
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

        let (ranges, len) = filled_ranges(self.data.len(), self.mask, r, w);
        // SAFETY: ranges are guaranteed to not overlap with any ranges
        //         `synced_read` will use at the same time.
        let bufs = unsafe { self.data.slices(ranges) };
        let n = f(bufs, len)?;
        debug_assert!(n <= len, "{n} <= {len}");

        self.read.store(r.checked_add(n).unwrap(), Release);
        Ok(n)
    }
}

#[must_use]
#[inline]
const fn filled_ranges(
    buflen: usize,
    mask: usize,
    read: usize,
    write: usize,
) -> ([Range<usize>; 2], usize) {
    debug_assert!(read <= write);
    debug_assert!(write <= read + buflen);

    let len = write - read;
    let start = read & mask;
    let end = start + len;
    let endw = end & mask;

    let ranges = if end == endw {
        [start..end, 0..0]
    } else {
        [start..buflen, 0..endw]
    };

    debug_assert!(range_len(&ranges[0]) + range_len(&ranges[1]) == len);
    (ranges, len)
}

#[must_use]
#[inline]
const fn empty_ranges(
    buflen: usize,
    mask: usize,
    read: usize,
    write: usize,
) -> ([Range<usize>; 2], usize) {
    debug_assert!(read <= write);
    debug_assert!(write <= read + buflen);

    let len = buflen - (write - read);
    let start = write & mask;
    let end = start + len;
    let endw = end & mask;

    let ranges = if end == endw {
        [start..end, 0..0]
    } else {
        [start..buflen, 0..endw]
    };

    debug_assert!(range_len(&ranges[0]) + range_len(&ranges[1]) == len);
    (ranges, len)
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
    #[must_use]
    #[inline]
    pub fn position(&self) -> usize {
        self.buffer.write.load(Relaxed)
    }
}

// TODO: impl write_vectored
#[cfg(feature = "std")]
impl io::Write for Reader {
    #[inline]
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        use io::Read;
        let mut src = io::Cursor::new(src);
        self.io_slices(move |dsts, _| src.read_vectored(dsts))
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
    #[must_use]
    #[inline]
    pub fn position(&self) -> usize {
        self.buffer.read.load(Relaxed)
    }

    #[doc(hidden)]
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        let r = self.buffer.read.load(Relaxed);
        let w = self.buffer.write.load(Relaxed);
        w == r
    }
}

// TODO: impl write_vectored
#[cfg(feature = "std")]
impl io::Read for Writer {
    #[inline]
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        use io::Write;
        let mut dst = io::Cursor::new(dst);
        self.io_slices(move |srcs, _| dst.write_vectored(srcs))
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
        //         A null pointer is immediately handled.
        let ptr = unsafe { NonNull::new(alloc_zeroed(layout)).unwrap() };

        let addr = ptr.as_ptr() as usize;
        assert_eq!(addr % align, 0, "aligned alloc failed");

        AlignedData { ptr, layout }
    }

    #[must_use]
    #[inline]
    fn len(&self) -> usize {
        self.layout.size()
    }

    /// # Safety
    /// * The passed ranges must both define non-overlapping regions of the
    ///   allocated data.
    /// * The passed ranges must not overlap with any other ranges passed to
    ///   `slices` or `slices_mut` at the same time.
    #[must_use]
    #[inline]
    unsafe fn slices(&self, ranges: [Range<usize>; 2]) -> [&[u8]; 2] {
        // SAFETY: the pointer is acquired through alloc_zeroed and is checked
        //         to be non-null. Provided the safety rules of the method are
        //         followed then the added pointer offset and the used length
        //         map a valid region of the allocated data.
        unsafe {
            ranges.map(|s| {
                debug_assert!(s.end <= self.len());
                &*ptr::slice_from_raw_parts(self.ptr.as_ptr().add(s.start), range_len(&s))
            })
        }
    }

    /// # Safety
    /// * The passed ranges must both define non-overlapping regions of the
    ///   allocated data.
    /// * The passed ranges must not overlap with any other ranges passed to
    ///   `slices` or `slices_mut` at the same time.
    #[must_use]
    #[inline]
    unsafe fn slices_mut(&self, ranges: [Range<usize>; 2]) -> [&mut [u8]; 2] {
        // SAFETY: the pointer is acquired through alloc_zeroed and is checked
        //         to be non-null. Provided the safety rules of the method are
        //         followed then the added pointer offset and the used length
        //         map a valid region of the allocated data.
        unsafe {
            ranges.map(|s| {
                debug_assert!(s.end <= self.len());
                &mut *ptr::slice_from_raw_parts_mut(self.ptr.as_ptr().add(s.start), range_len(&s))
            })
        }
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

const fn range_len(r: &Range<usize>) -> usize {
    debug_assert!(r.start <= r.end);
    r.end - r.start
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
    fn test_filled_ranges() {
        let data = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mask = 15;

        let (ranges, len) = filled_ranges(data.len(), mask, 2, 13);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 11);

        let (ranges, len) = filled_ranges(data.len(), mask, 17, 20);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[1, 2, 3]);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 3);

        let (ranges, len) = filled_ranges(data.len(), mask, 10, 20);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[10, 11, 12, 13, 14, 15]);
        assert_eq!(bufs[1], &[0, 1, 2, 3]);
        assert_eq!(len, 10);

        let (ranges, len) = filled_ranges(data.len(), mask, 16, 20);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[0, 1, 2, 3]);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 4);

        let (ranges, len) = filled_ranges(data.len(), mask, 0, 16);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(
            bufs[0],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 16);

        let (ranges, len) = filled_ranges(data.len(), mask, 0, 0);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);

        let (ranges, len) = filled_ranges(data.len(), mask, 15, 15);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);

        let (ranges, len) = filled_ranges(data.len(), mask, 16, 16);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);
    }

    #[test]
    fn test_empty_ranges() {
        let data = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mask = 15;

        let (ranges, len) = empty_ranges(data.len(), mask, 2, 13);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[13, 14, 15]);
        assert_eq!(bufs[1], &[0, 1]);
        assert_eq!(len, 5);

        let (ranges, len) = empty_ranges(data.len(), mask, 13, 17);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(bufs[1], &[]);
        assert_eq!(len, 12);

        let (ranges, len) = empty_ranges(data.len(), mask, 0, 16);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0].len(), 0);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 0);

        let (ranges, len) = empty_ranges(data.len(), mask, 0, 0);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(
            bufs[0],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 16);

        let (ranges, len) = empty_ranges(data.len(), mask, 15, 15);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[15]);
        assert_eq!(bufs[1], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
        assert_eq!(len, 16);

        let (ranges, len) = empty_ranges(data.len(), mask, 16, 16);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(
            bufs[0],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 16);
    }
}
