#![deny(
    clippy::undocumented_unsafe_blocks,
    deprecated,
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

use ::alloc::alloc::{Layout, alloc_zeroed, dealloc};
use ::alloc::sync::Arc;
use ::core::clone::Clone;
use ::core::default::Default as _;
use ::core::fmt;
use ::core::hint;
use ::core::marker::{PhantomData, Send, Sync};
use ::core::ops::{Drop, FnMut, Range};
use ::core::option::Option::{self, None, Some};
use ::core::ptr::{self, NonNull};
use ::core::result::Result::{self, Err, Ok};
use ::core::sync::atomic::AtomicUsize;
use ::core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use ::core::{debug_assert, write};
use ::crossbeam_utils::CachePadded;
#[cfg(feature = "std")]
use ::std::io;

/// Creates a producer-consumer pair sharing a ring buffer.
///
/// # Errors
///
/// Returns an error when `size` or `align` is not a power of two, or when
/// the allocation fails.
#[inline]
pub fn new(size: usize, align: usize) -> Result<(Producer, Consumer), BufferError> {
    // implies != 0
    if !size.is_power_of_two() {
        return Err(BufferError::BadSize(size));
    }
    if !align.is_power_of_two() {
        return Err(BufferError::BadAlignment(align));
    }

    let mask = size.wrapping_sub(1);

    let data = AlignedData::new(size, align)?;

    let buffer = Arc::new(Buffer {
        read: CachePadded::default(),
        write: CachePadded::default(),
        mask,
        data,
    });

    let producer = Producer {
        buffer: Arc::clone(&buffer),
        _notsync: PhantomData,
    };
    let consumer = Consumer {
        buffer,
        _notsync: PhantomData,
    };

    Ok((producer, consumer))
}

// TODO: put data and counters into same heap allocation. This would also
//       remove the `Arc` allocation and with it the only remaining abort
//       path: `Arc::new` calls `handle_alloc_error` when out of memory.
#[derive(Debug)]
struct Buffer {
    read: CachePadded<AtomicUsize>,
    write: CachePadded<AtomicUsize>,
    mask: usize,
    data: AlignedData,
}

// SAFETY: Sync is safe because the slices handed out over `data` are never
//         aliased: each counter is advanced only through its uniquely owned,
//         non-`Clone` half (`Producer` for `write`, `Consumer` for `read`), the
//         slice-vending methods take `&mut self` so a half cannot re-enter
//         them while its slices are live, and the counter protocol keeps the
//         producer's empty ranges and the consumer's filled ranges disjoint.
//         A callback's returned count is checked against the offered length
//         before a counter is advanced, so the wrapping distance
//         `write - read` stays within `0..=size` even with a buggy callback.
unsafe impl Sync for Buffer {}

impl Buffer {
    #[inline]
    fn produce_fn<E>(
        &self,
        mut f: impl FnMut([&mut [u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, ProducerError<E>> {
        let w = self.write.load(Relaxed);
        let r = self.read.load(Acquire);

        let (ranges, len) = empty_ranges(self.data.len(), self.mask, r, w);
        if len == 0 {
            // TODO: feature gated WouldBlock
        }

        // SAFETY: ranges map the empty region only, which is guaranteed to
        //         not overlap with the filled region `consume_fn` uses at the
        //         same time.
        let bufs = unsafe { self.data.slices_mut(ranges) };

        let n = f(bufs, len).map_err(ProducerError::Callback)?;
        if n > len {
            hint::cold_path();
            return Err(ProducerError::InvalidCount { n, len });
        }

        if n != 0 {
            self.write.store(w.wrapping_add(n), Release);
        }

        Ok(n)
    }

    #[inline]
    fn consume_fn<E>(
        &self,
        mut f: impl FnMut([&[u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, ConsumerError<E>> {
        let r = self.read.load(Relaxed);
        let w = self.write.load(Acquire);

        let (ranges, len) = filled_ranges(self.data.len(), self.mask, r, w);
        if len == 0 {
            // TODO: feature gated WouldBlock
        }

        // SAFETY: ranges map the filled region only, which is guaranteed to
        //         not overlap with the empty region `produce_fn` uses at the
        //         same time.
        let bufs = unsafe { self.data.slices(ranges) };

        let n = f(bufs, len).map_err(ConsumerError::Callback)?;
        if n > len {
            hint::cold_path();
            return Err(ConsumerError::InvalidCount { n, len });
        }

        if n != 0 {
            self.read.store(r.wrapping_add(n), Release);
        }

        Ok(n)
    }
}

/// The error type returned by the slice-vending methods of [`Producer`].
#[derive(Debug, Clone)]
pub enum ProducerError<E> {
    /// The callback (the closure or function passed in) failed. Its error
    /// is passed through unchanged.
    Callback(E),
    /// The callback returned a count exceeding the length it was offered.
    /// The count was discarded and the buffer is unchanged, so the half
    /// stays usable.
    InvalidCount {
        /// The count the callback returned.
        n: usize,
        /// The total length the callback was offered.
        len: usize,
    },
}

impl<E: fmt::Display> fmt::Display for ProducerError<E> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProducerError::Callback(e) => fmt::Display::fmt(e, f),
            ProducerError::InvalidCount { n, len } => {
                write!(
                    f,
                    "callback returned a count of {n}, but only {len} bytes were available"
                )
            }
        }
    }
}

impl<E: ::core::error::Error> ::core::error::Error for ProducerError<E> {
    #[inline]
    fn source(&self) -> Option<&(dyn ::core::error::Error + 'static)> {
        match self {
            ProducerError::Callback(e) => e.source(),
            ProducerError::InvalidCount { .. } => None,
        }
    }
}

/// The error type returned by the slice-vending methods of [`Consumer`].
#[derive(Debug, Clone)]
pub enum ConsumerError<E> {
    /// The callback (the closure or function passed in) failed. Its error
    /// is passed through unchanged.
    Callback(E),
    /// The callback returned a count exceeding the length it was offered.
    /// The count was discarded and the buffer is unchanged, so the half
    /// stays usable.
    InvalidCount {
        /// The count the callback returned.
        n: usize,
        /// The total length the callback was offered.
        len: usize,
    },
}

impl<E: fmt::Display> fmt::Display for ConsumerError<E> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConsumerError::Callback(e) => fmt::Display::fmt(e, f),
            ConsumerError::InvalidCount { n, len } => {
                write!(
                    f,
                    "callback returned a count of {n}, but only {len} bytes were available"
                )
            }
        }
    }
}

impl<E: ::core::error::Error> ::core::error::Error for ConsumerError<E> {
    #[inline]
    fn source(&self) -> Option<&(dyn ::core::error::Error + 'static)> {
        match self {
            ConsumerError::Callback(e) => e.source(),
            ConsumerError::InvalidCount { .. } => None,
        }
    }
}

/// The error type returned by [`new`].
#[derive(Debug, Clone)]
pub enum BufferError {
    /// The requested size is not a power of two, or too large to allocate.
    BadSize(usize),
    /// The requested alignment is not a power of two.
    BadAlignment(usize),
    /// The allocator failed to provide the requested memory.
    AllocFailed,
}

impl fmt::Display for BufferError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BufferError::BadSize(size) => {
                write!(f, "size is not a power of two or too large: {size}")
            }
            BufferError::BadAlignment(align) => {
                write!(f, "alignment is not a power of two: {align}")
            }
            BufferError::AllocFailed => write!(f, "allocation failed"),
        }
    }
}

impl ::core::error::Error for BufferError {}

#[must_use]
#[inline]
const fn filled_ranges(
    buflen: usize,
    mask: usize,
    read: usize,
    write: usize,
) -> ([Range<usize>; 2], usize) {
    debug_assert!(write.wrapping_sub(read) <= buflen);

    let len = write.wrapping_sub(read);
    let start = read & mask;
    let end = start.wrapping_add(len);
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
    debug_assert!(write.wrapping_sub(read) <= buflen);

    let len = buflen.wrapping_sub(write.wrapping_sub(read));
    let start = write & mask;
    let end = start.wrapping_add(len);
    let endw = end & mask;

    let ranges = if end == endw {
        [start..end, 0..0]
    } else {
        [start..buflen, 0..endw]
    };

    debug_assert!(range_len(&ranges[0]) + range_len(&ranges[1]) == len);
    (ranges, len)
}

/// Keeps the containing half `Send` while suppressing `Sync`, without an
/// unsafe impl.
type SendNotSyncZst = ::core::cell::Cell<()>;

#[derive(Debug)]
pub struct Producer {
    buffer: Arc<Buffer>,
    _notsync: PhantomData<SendNotSyncZst>,
}

impl Producer {
    /// Fills the buffer: calls the passed closure with a pair of
    /// [`io::IoSliceMut`] mapping the empty space, meant to be used with
    /// [`io::Read::read_vectored`] and async variants, and the total length
    /// of both slices.
    /// The closure must return the number of bytes read on success.
    ///
    /// # Errors
    ///
    /// Returns [`ProducerError::Callback`] with the closure's error
    /// unchanged, or [`ProducerError::InvalidCount`] if the closure returned
    /// a count greater than the total length it was given.
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_slices(
        &mut self,
        mut f: impl FnMut(&mut [io::IoSliceMut<'_>], usize) -> io::Result<usize>,
    ) -> Result<usize, ProducerError<io::Error>> {
        self.buffer.produce_fn(|bufs, len| {
            let mut bufs = bufs.map(io::IoSliceMut::new);
            f(&mut bufs, len)
        })
    }

    /// Fills the buffer: calls the passed closure with a pair of `&mut [u8]`
    /// mapping the empty space, meant to be used with non `std::io` vectored
    /// read operations.
    ///
    /// # Errors
    ///
    /// Returns [`ProducerError::Callback`] with the closure's error
    /// unchanged, or [`ProducerError::InvalidCount`] if the closure returned
    /// a count greater than the total length it was given.
    #[inline]
    pub fn slices<E>(
        &mut self,
        mut f: impl FnMut(&mut [&mut [u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, ProducerError<E>> {
        self.buffer.produce_fn(|mut bufs, len| f(&mut bufs, len))
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
impl io::Write for Producer {
    #[inline]
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        use io::Read;
        let mut src = io::Cursor::new(src);
        match self.io_slices(move |dsts, _| src.read_vectored(dsts)) {
            Ok(n) => Ok(n),
            Err(ProducerError::Callback(e)) => Err(e),
            Err(err @ ProducerError::InvalidCount { .. }) => Err(io::Error::other(err)),
        }
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct Consumer {
    buffer: Arc<Buffer>,
    _notsync: PhantomData<SendNotSyncZst>,
}

impl Consumer {
    /// Drains the buffer: calls the passed closure with a pair of
    /// [`io::IoSlice`] mapping the filled space, meant to be used with
    /// [`io::Write::write_vectored`] and async variants, and the total length
    /// of both slices.
    /// The closure must return the number of bytes written on success.
    ///
    /// # Errors
    ///
    /// Returns [`ConsumerError::Callback`] with the closure's error
    /// unchanged, or [`ConsumerError::InvalidCount`] if the closure returned
    /// a count greater than the total length it was given.
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_slices(
        &mut self,
        mut f: impl FnMut(&[io::IoSlice<'_>], usize) -> io::Result<usize>,
    ) -> Result<usize, ConsumerError<io::Error>> {
        self.buffer.consume_fn(|bufs, len| {
            let bufs = bufs.map(io::IoSlice::new);
            f(&bufs, len)
        })
    }

    /// Drains the buffer: calls the passed closure with a pair of `&[u8]`
    /// mapping the filled space, meant to be used with non `std::io` vectored
    /// write operations.
    ///
    /// # Errors
    ///
    /// Returns [`ConsumerError::Callback`] with the closure's error
    /// unchanged, or [`ConsumerError::InvalidCount`] if the closure returned
    /// a count greater than the total length it was given.
    #[inline]
    pub fn slices<E>(
        &mut self,
        mut f: impl FnMut(&[&[u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, ConsumerError<E>> {
        self.buffer.consume_fn(|bufs, len| f(&bufs, len))
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
impl io::Read for Consumer {
    #[inline]
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        use io::Write;
        let mut dst = io::Cursor::new(dst);
        match self.io_slices(move |srcs, _| dst.write_vectored(srcs)) {
            Ok(n) => Ok(n),
            Err(ConsumerError::Callback(e)) => Err(e),
            Err(err @ ConsumerError::InvalidCount { .. }) => Err(io::Error::other(err)),
        }
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
    fn new(size: usize, align: usize) -> Result<Self, BufferError> {
        debug_assert!(size != 0, "size cannot be zero");

        let Ok(layout) = Layout::from_size_align(size, align) else {
            return Err(BufferError::BadSize(size));
        };

        // SAFETY: alloc is called with a correct layout with a non-zero size.
        //         A null pointer is handled right below.
        let Some(ptr) = NonNull::new(unsafe { alloc_zeroed(layout) }) else {
            return Err(BufferError::AllocFailed);
        };

        debug_assert!(
            (ptr.as_ptr() as usize).is_multiple_of(align),
            "aligned alloc failed"
        );

        Ok(AlignedData { ptr, layout })
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
    #[expect(
        clippy::mut_from_ref,
        reason = "aliasing is ruled out by the # Safety contract"
    )]
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
    r.end.wrapping_sub(r.start)
}

#[cfg(test)]
mod tests {
    use ::core::cmp::Ord;
    use ::core::convert::{From as _, TryFrom as _};
    use ::core::marker::{Send, Sized, Sync};
    use ::core::{assert, assert_eq, matches};
    use ::static_assertions::{assert_impl_all, assert_not_impl_any};

    use super::*;

    assert_impl_all!(Buffer: Send, Sync);
    assert_impl_all!(Producer: Send);
    assert_not_impl_any!(Producer: Sync);
    assert_impl_all!(Consumer: Send);
    assert_not_impl_any!(Consumer: Sync);

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

        // The write counter has wrapped around zero, the read counter has
        // not yet: write is numerically smaller than read.
        let read = usize::MAX - 5;
        let write = read.wrapping_add(9);
        let (ranges, len) = filled_ranges(data.len(), mask, read, write);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[10, 11, 12, 13, 14, 15]);
        assert_eq!(bufs[1], &[0, 1, 2]);
        assert_eq!(len, 9);
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

        // The write counter has wrapped around zero, the read counter has
        // not yet: write is numerically smaller than read.
        let read = usize::MAX - 5;
        let write = read.wrapping_add(9);
        let (ranges, len) = empty_ranges(data.len(), mask, read, write);
        let bufs = ranges.map(|r| &data[r]);
        assert_eq!(bufs[0], &[3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(bufs[1].len(), 0);
        assert_eq!(len, 7);
    }

    const RING: usize = 16;

    /// Builds a pair over a 16-byte ring whose counters both start at
    /// `start`, to exercise arbitrary counter positions.
    fn seeded_pair(start: usize) -> (Producer, Consumer) {
        let buffer = Arc::new(Buffer {
            read: CachePadded::new(AtomicUsize::new(start)),
            write: CachePadded::new(AtomicUsize::new(start)),
            mask: RING - 1,
            data: AlignedData::new(RING, RING).unwrap(),
        });
        let producer = Producer {
            buffer: Arc::clone(&buffer),
            _notsync: PhantomData,
        };
        let consumer = Consumer {
            buffer,
            _notsync: PhantomData,
        };
        (producer, consumer)
    }

    /// Pumps `total` bytes of a position-dependent pattern through the pair
    /// with odd chunk sizes, verifying every byte on the way out.
    fn pump_pattern(producer: &mut Producer, consumer: &mut Consumer, total: usize) {
        const PRODUCE_MAX: usize = 11;
        const CONSUME_MAX: usize = 7;

        let mut written = 0;
        let mut read = 0;

        while read < total {
            if written < total {
                let n = producer
                    .slices(|bufs, len| {
                        let cap = len.min(PRODUCE_MAX).min(total - written);
                        let mut n = 0;
                        'bufs: for buf in bufs.iter_mut() {
                            for b in buf.iter_mut() {
                                if n == cap {
                                    break 'bufs;
                                }
                                *b = u8::try_from((written + n) % 251).unwrap();
                                n += 1;
                            }
                        }
                        Ok::<_, ()>(n)
                    })
                    .unwrap();
                assert!(n > 0, "producer must make progress");
                written += n;
            }

            let n = consumer
                .slices(|bufs, len| {
                    let cap = len.min(CONSUME_MAX);
                    let mut n = 0;
                    'bufs: for buf in bufs {
                        for &b in *buf {
                            if n == cap {
                                break 'bufs;
                            }
                            assert_eq!(usize::from(b), (read + n) % 251, "byte {}", read + n);
                            n += 1;
                        }
                    }
                    Ok::<_, ()>(n)
                })
                .unwrap();
            assert!(n > 0, "consumer must make progress");
            read += n;
        }

        assert_eq!(written, total);
        assert_eq!(read, total);
    }

    #[test]
    fn roundtrip_pattern_across_wraps() {
        const TOTAL: usize = 200;

        let (mut producer, mut consumer) = new(RING, RING).unwrap();
        pump_pattern(&mut producer, &mut consumer, TOTAL);

        assert_eq!(producer.position(), TOTAL);
        assert_eq!(consumer.position(), TOTAL);
        assert!(consumer.is_empty());
    }

    #[test]
    fn roundtrip_across_counter_wrap() {
        const TOTAL: usize = 200;
        // The write counter wraps around zero mid-run while the read counter
        // is still near the top of the usize range.
        const START: usize = usize::MAX - 99;

        let (mut producer, mut consumer) = seeded_pair(START);
        pump_pattern(&mut producer, &mut consumer, TOTAL);

        assert_eq!(producer.position(), START.wrapping_add(TOTAL));
        assert_eq!(consumer.position(), START.wrapping_add(TOTAL));
        assert!(consumer.is_empty());
    }

    #[test]
    fn producer_invalid_count_errors() {
        let (mut producer, _consumer) = new(16, 16).unwrap();
        let res = producer.slices(|_bufs, len| Ok::<_, ()>(len + 1));
        assert!(matches!(
            res,
            Err(ProducerError::InvalidCount { n: 17, len: 16 })
        ));

        // The invalid count was discarded; the half stays usable.
        let n = producer.slices(|_bufs, len| Ok::<_, ()>(len)).unwrap();
        assert_eq!(n, 16);
        assert_eq!(producer.position(), 16);
    }

    #[test]
    fn consumer_invalid_count_errors() {
        let (mut producer, mut consumer) = new(16, 16).unwrap();
        producer.slices(|_bufs, _len| Ok::<_, ()>(4)).unwrap();
        let res = consumer.slices(|_bufs, len| Ok::<_, ()>(len + 1));
        assert!(matches!(
            res,
            Err(ConsumerError::InvalidCount { n: 5, len: 4 })
        ));

        // The invalid count was discarded; the half stays usable.
        let n = consumer.slices(|_bufs, len| Ok::<_, ()>(len)).unwrap();
        assert_eq!(n, 4);
        assert_eq!(consumer.position(), 4);
    }

    #[test]
    fn callback_error_passes_through() {
        let (mut producer, _consumer) = new(16, 16).unwrap();
        let res = producer.slices(|_bufs, _len| Err::<usize, i32>(-1));
        assert!(matches!(res, Err(ProducerError::Callback(-1))));
        assert_eq!(producer.position(), 0);
    }

    #[test]
    fn new_rejects_bad_parameters() {
        assert!(matches!(new(0, 16), Err(BufferError::BadSize(0))));
        assert!(matches!(new(15, 16), Err(BufferError::BadSize(15))));
        assert!(matches!(new(16, 15), Err(BufferError::BadAlignment(15))));
    }
}
