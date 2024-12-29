# `bytering::Buffer`

A simple ring buffer with fixed capacity specialized for vectored reading and
writing in blocking and async I/O.

Similar to `VecDeque` it provides a pair of byte slices in correct order to the
filled regions of its internal linear space. Unlike `VecDeque` it also provides
pairs of mutable slices for writing into the empty region or regions. These
pairs of slices can be directly passed to `read_vectored` and `write_vectored`.

If neither source nor target of data has optimized vectored I/O this ring buffer
does not offer any advantages.
