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
)]
// TODO: document everything
#![expect(
    clippy::undocumented_unsafe_blocks,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
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
use ::core::{assert, assert_eq, slice};
#[cfg(feature = "std")]
use ::std::io;

#[derive(Debug)]
pub struct Buffer {
    inner: Arc<BufferInner>,
}

impl Buffer {
    #[must_use]
    #[inline]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "capacity is not power of two: {capacity}"
        );
        Buffer {
            inner: Arc::new(BufferInner {
                read: AtomicUsize::new(0),
                write: AtomicUsize::new(0),
                mask: capacity - 1,
                data: UnsafeCell::new(AlignedData::new(capacity, 4096)),
            }),
        }
    }

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

// TODO: make data sync, if possible
unsafe impl Sync for BufferInner {}

impl BufferInner {
    #[must_use]
    #[inline]
    fn data(&self) -> &[u8] {
        unsafe { &*(self.data.get()) }
    }

    #[allow(clippy::mut_from_ref)]
    #[must_use]
    #[inline]
    unsafe fn data_mut(&self) -> &mut [u8] {
        unsafe { &mut *self.data.get() }
    }

    #[inline]
    fn synced_read<E>(
        &self,
        mut f: impl FnMut([&mut [u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let r = self.read.load(Relaxed);
        let w = self.write.load(Acquire);

        let (bufs, len) = self.empty_segments(r, w);
        let n = f(bufs, len)?;
        assert!(n <= len);

        self.write.store(w + n, Release);
        Ok(n)
    }

    #[must_use]
    #[inline]
    fn empty_segments(&self, read: usize, write: usize) -> ([&mut [u8]; 2], usize) {
        let (start, end) = (read & self.mask, write & self.mask);
        let data = unsafe { self.data_mut() };
        let len = data.len() - (write - read);
        if start > end {
            ([&mut data[end..start], &mut []], len)
        } else {
            let (b, a) = data.split_at_mut(end);
            ([a, &mut b[..start]], len)
        }
    }

    #[inline]
    fn synced_write<E>(
        &self,
        mut f: impl FnMut([&[u8]; 2], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let r = self.read.load(Relaxed);
        let w = self.write.load(Acquire);

        let (bufs, len) = self.filled_segments(r, w);
        let n = f(bufs, len)?;
        assert!(n <= len);

        self.read.store(r + n, Release);
        Ok(n)
    }

    #[must_use]
    #[inline]
    fn filled_segments(&self, read: usize, write: usize) -> ([&[u8]; 2], usize) {
        let (start, end) = (read & self.mask, write & self.mask);
        let data = self.data();
        let len = write - read;
        if start < end {
            ([&data[start..end], &[]], len)
        } else {
            let (b, a) = data.split_at(start);
            ([a, &b[..end]], len)
        }
    }
}

#[derive(Debug)]
pub struct Reader {
    buffer: Arc<BufferInner>,
    _notsync: PhantomData<*mut ()>,
}

unsafe impl Send for Reader {}

impl Reader {
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_read_from(
        &self,
        mut f: impl FnMut(&mut [io::IoSliceMut<'_>], usize) -> io::Result<usize>,
    ) -> io::Result<usize> {
        self.buffer.synced_read(|bufs, len| {
            let mut bufs = bufs.map(io::IoSliceMut::new);
            f(&mut bufs, len)
        })
    }

    #[inline]
    pub fn read_from<E>(
        &self,
        mut f: impl FnMut(&mut [&mut [u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        self.buffer.synced_read(|mut bufs, len| f(&mut bufs, len))
    }
}

#[derive(Debug)]
pub struct Writer {
    buffer: Arc<BufferInner>,
    _notsync: PhantomData<*mut ()>,
}

unsafe impl Send for Writer {}

impl Writer {
    #[cfg(feature = "std")]
    #[inline]
    pub fn io_write_into(
        &self,
        mut f: impl FnMut(&[io::IoSlice<'_>], usize) -> io::Result<usize>,
    ) -> io::Result<usize> {
        self.buffer.synced_write(|bufs, len| {
            let bufs = bufs.map(io::IoSlice::new);
            f(&bufs, len)
        })
    }

    #[inline]
    pub fn write_into<E>(
        &self,
        mut f: impl FnMut(&[&[u8]], usize) -> Result<usize, E>,
    ) -> Result<usize, E> {
        self.buffer.synced_write(|bufs, len| f(&bufs, len))
    }
}

#[derive(Debug)]
struct AlignedData {
    ptr: NonNull<u8>,
    layout: Layout,
}

unsafe impl Send for AlignedData {}

impl AlignedData {
    #[inline]
    fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).unwrap();
        let ptr = unsafe { alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap();

        let addr = ptr.as_ptr() as usize;
        assert_eq!(addr % align, 0, "aligned alloc failed");

        AlignedData { ptr, layout }
    }

    #[must_use]
    #[inline]
    fn slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }

    #[must_use]
    #[inline]
    fn slice_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedData {
    #[inline]
    fn drop(&mut self) {
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
}
