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
use ::core::option::Option::{self, None};
use ::core::ptr::{self, NonNull};
use ::core::result::Result::{self, Err, Ok};
use ::core::sync::atomic::AtomicUsize;
use ::core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use ::core::{assert, assert_eq, assert_ne, debug_assert, write};
use ::crossbeam_utils::CachePadded;
#[cfg(feature = "std")]
use ::std::io;

/// Creates a producer-consumer pair sharing a ring buffer.
///
/// # Panics
///
/// Will panic if `size` or `align` is not a power of 2.
#[must_use]
#[inline]
pub fn new(size: usize, align: usize) -> (Producer, Consumer) {
    assert!(
        size.is_power_of_two(), // implies != 0
        "size is not power of two: {size}"
    );
    // TODO: consider accepting any modulus and let compiler optimize
    //       this into and-ing for power-of-2s.
    let mask = size - 1;

    let data = AlignedData::new(size, align);

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

    (producer, consumer)
}

// TODO: put data and counters into same heap allocation.
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
//         before a counter is advanced, so `read <= write <= read + size`
//         holds even with a buggy callback.
unsafe impl Sync for Buffer {}

impl Buffer {
    #[inline]
    fn produce_fn<E>(
        &self,
        mut f: impl FnMut([&mut [u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, BufferError<E>> {
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

        let n = f(bufs, len).map_err(BufferError::Callback)?;
        if n > len {
            hint::cold_path();
            return Err(BufferError::InvalidCount { n, len });
        }

        self.write.store(w.checked_add(n).unwrap(), Release);
        Ok(n)
    }

    #[inline]
    fn consume_fn<E>(
        &self,
        mut f: impl FnMut([&[u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, BufferError<E>> {
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

        let n = f(bufs, len).map_err(BufferError::Callback)?;
        if n > len {
            hint::cold_path();
            return Err(BufferError::InvalidCount { n, len });
        }

        self.read.store(r.checked_add(n).unwrap(), Release);
        Ok(n)
    }
}

/// The error type returned by the slice-vending methods of [`Producer`] and
/// [`Consumer`].
#[derive(Debug, Clone)]
pub enum BufferError<E> {
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

impl<E: fmt::Display> fmt::Display for BufferError<E> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BufferError::Callback(e) => fmt::Display::fmt(e, f),
            BufferError::InvalidCount { n, len } => {
                write!(
                    f,
                    "callback returned a count of {n}, but only {len} bytes were available"
                )
            }
        }
    }
}

impl<E: ::core::error::Error> ::core::error::Error for BufferError<E> {
    #[inline]
    fn source(&self) -> Option<&(dyn ::core::error::Error + 'static)> {
        match self {
            BufferError::Callback(e) => e.source(),
            BufferError::InvalidCount { .. } => None,
        }
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
    /// Returns [`BufferError::Callback`] with the closure's error unchanged, or
    /// [`BufferError::InvalidCount`] if the closure returned a count greater
    /// than the total length it was given.
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_slices(
        &mut self,
        mut f: impl FnMut(&mut [io::IoSliceMut<'_>], usize) -> io::Result<usize>,
    ) -> Result<usize, BufferError<io::Error>> {
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
    /// Returns [`BufferError::Callback`] with the closure's error unchanged, or
    /// [`BufferError::InvalidCount`] if the closure returned a count greater
    /// than the total length it was given.
    #[inline]
    pub fn slices<E>(
        &mut self,
        mut f: impl FnMut(&mut [&mut [u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, BufferError<E>> {
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
            Err(BufferError::Callback(e)) => Err(e),
            Err(err @ BufferError::InvalidCount { .. }) => Err(io::Error::other(err)),
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
    /// Returns [`BufferError::Callback`] with the closure's error unchanged, or
    /// [`BufferError::InvalidCount`] if the closure returned a count greater
    /// than the total length it was given.
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_slices(
        &mut self,
        mut f: impl FnMut(&[io::IoSlice<'_>], usize) -> io::Result<usize>,
    ) -> Result<usize, BufferError<io::Error>> {
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
    /// Returns [`BufferError::Callback`] with the closure's error unchanged, or
    /// [`BufferError::InvalidCount`] if the closure returned a count greater
    /// than the total length it was given.
    #[inline]
    pub fn slices<E>(
        &mut self,
        mut f: impl FnMut(&[&[u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, BufferError<E>> {
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
            Err(BufferError::Callback(e)) => Err(e),
            Err(err @ BufferError::InvalidCount { .. }) => Err(io::Error::other(err)),
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
    r.end - r.start
}

#[cfg(test)]
mod tests {
    use ::core::cmp::Ord;
    use ::core::convert::{From as _, TryFrom as _};
    use ::core::marker::{Send, Sized, Sync};
    use ::core::matches;
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

    #[test]
    fn roundtrip_pattern_across_wraps() {
        const RING: usize = 16;
        const TOTAL: usize = 200;
        const PRODUCE_MAX: usize = 11;
        const CONSUME_MAX: usize = 7;

        let (mut producer, mut consumer) = new(RING, RING);

        let mut written = 0;
        let mut read = 0;

        while read < TOTAL {
            if written < TOTAL {
                let n = producer
                    .slices(|bufs, len| {
                        let cap = len.min(PRODUCE_MAX).min(TOTAL - written);
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

        assert_eq!(written, TOTAL);
        assert_eq!(read, TOTAL);
        assert_eq!(producer.position(), TOTAL);
        assert_eq!(consumer.position(), TOTAL);
        assert!(consumer.is_empty());
    }

    #[test]
    fn producer_invalid_count_errors() {
        let (mut producer, _consumer) = new(16, 16);
        let res = producer.slices(|_bufs, len| Ok::<_, ()>(len + 1));
        assert!(matches!(
            res,
            Err(BufferError::InvalidCount { n: 17, len: 16 })
        ));

        // The invalid count was discarded; the half stays usable.
        let n = producer.slices(|_bufs, len| Ok::<_, ()>(len)).unwrap();
        assert_eq!(n, 16);
        assert_eq!(producer.position(), 16);
    }

    #[test]
    fn consumer_invalid_count_errors() {
        let (mut producer, mut consumer) = new(16, 16);
        producer.slices(|_bufs, _len| Ok::<_, ()>(4)).unwrap();
        let res = consumer.slices(|_bufs, len| Ok::<_, ()>(len + 1));
        assert!(matches!(
            res,
            Err(BufferError::InvalidCount { n: 5, len: 4 })
        ));

        // The invalid count was discarded; the half stays usable.
        let n = consumer.slices(|_bufs, len| Ok::<_, ()>(len)).unwrap();
        assert_eq!(n, 4);
        assert_eq!(consumer.position(), 4);
    }

    #[test]
    fn callback_error_passes_through() {
        let (mut producer, _consumer) = new(16, 16);
        let res = producer.slices(|_bufs, _len| Err::<usize, i32>(-1));
        assert!(matches!(res, Err(BufferError::Callback(-1))));
        assert_eq!(producer.position(), 0);
    }
}
