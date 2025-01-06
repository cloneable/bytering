# `bytering::Buffer`

A simple, lock-free ring buffer for bytes with fixed capacity. It's specialized
for low latency, vectored reading and writing in blocking and async I/O
operations.

Similar to `VecDeque` it provides a pair of byte slices mapping the filled space
of its internal linear space. Unlike `VecDeque` it provides a pair of mutable
slices for writing into the empty space. These pairs of slices can be directly
passed to `read_vectored` (`readv`) and `write_vectored` (`writev`),
respectively.

If neither source nor target of data supports direct vectored I/O this ring
buffer does not offer any advantages.

## Usage

```rust
let buffer = bytering::Buffer::new(4096, 4096);
let (reader, writer) = buffer.into_parts();

let r = reader.io_slices(|bufs, _len| {
    let r = input.read_vectored(bufs)?;
    Ok(r)
})?;
// -or- reader.io_slices(|bufs, _| input.read_vectored(bufs))?;

let w = writer.io_slices(|bufs, _len| {
    let w = output.write_vectored(bufs)?;
    Ok(w)
})?;
// -or- writer.io_slices(|bufs, _| output.write_vectored(bufs))?;
```

## Locking

The buffer is split into one reader half and one writer half after creation.
Each half controls one atomic counter: either the read counter or the write
counter. The counters are only ever incremented and the read counter cannot go
past the write counter and the write counter cannot go further away from the
read counter than the size of the buffer, ensuring that neither half accesses
memory currently "held" by the other half.

## Safety

The code contains some unsafe blocks:

* Allocating aligned memory requires accessing `alloc` and `dealloc` functions.
* Accessing the allocated memory is done by creating slices with
  `from_raw_parts(_mut)`.
* The empty segments of the buffer must be made mutable for writing. This is
  currently done via `UnsafeCell`.

## Caveats

* The buffer is split into one reader half and one writer half after creation.
  Neither half implements `Clone` nor `Sync`.
* It uses 64-bit counters that never reset. If you plan on pushing exabytes of
  data between restarts, this buffer is not for you.
* Its capacity must be a power of 2. This might change.
