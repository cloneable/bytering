# `bytering`

A simple, lock-free, SPSC ring buffer for bytes with fixed capacity. It's
specialized for low latency, vectored reading and writing in blocking and
async I/O operations. The read and write paths are panic-free.

Similar to `VecDeque` it provides a pair of byte slices mapping the filled space
of its internal linear space. Unlike `VecDeque` it provides a pair of mutable
slices for writing into the empty space. These pairs of slices can be directly
passed to `read_vectored` (`readv`) and `write_vectored` (`writev`),
respectively.

If neither source nor target of data supports direct vectored I/O this ring
buffer does not offer any advantages.

## Usage

```rust
let (mut producer, mut consumer) = bytering::new(4096, 4096).unwrap();

let r = producer.io_slices(|bufs, _len| {
    let r = input.read_vectored(bufs)?;
    Ok(r)
})?;

let w = consumer.io_slices(|bufs, _len| {
    let w = output.write_vectored(bufs)?;
    Ok(w)
})?;
```

## Locking

The buffer is split into one producer half and one consumer half after creation.
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

* The buffer is split into one producer half and one consumer half after creation.
  Neither half implements `Clone` nor `Sync`.
* Its capacity must be a power of 2. This might change.
