use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::{hint, thread};

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

fn main() -> io::Result<()> {
    const DATA_SIZE: usize = 10_000_000_000;

    let (reader, writer) = bytering::new(4096, 4096);

    let mut input = DummyInput {
        rng: SmallRng::seed_from_u64(12345),
        data: DATA_SIZE,
    };
    let mut output = DummyOutput {
        rng: SmallRng::seed_from_u64(54321),
        data: 0,
    };
    let done = Arc::new(AtomicBool::new(false));
    let done_check = Arc::clone(&done);

    let reader_thread: thread::JoinHandle<io::Result<_>> = thread::Builder::new()
        .name("reader".into())
        .spawn(move || {
            loop {
                let mut stop = false;
                reader.io_slices(|bufs, len| {
                    Ok(if len == 0 {
                        hint::spin_loop();
                        0
                    } else {
                        let n = input.read_vectored(bufs)?;
                        if n == 0 {
                            stop = true;
                        }
                        n
                    })
                })?;

                if stop {
                    done.store(true, Relaxed);
                    assert_eq!(reader.position(), DATA_SIZE);
                    return Ok(input);
                }
            }
        })?;

    let writer_thread: thread::JoinHandle<io::Result<_>> = thread::Builder::new()
        .name("writer".into())
        .spawn(move || {
            loop {
                writer.io_slices(|bufs, len| {
                    Ok(if len == 0 {
                        hint::spin_loop();
                        0
                    } else {
                        output.write_vectored(bufs)?
                    })
                })?;

                if writer.is_empty() && done_check.load(Relaxed) {
                    assert_eq!(writer.position(), DATA_SIZE);
                    return Ok(output);
                }
            }
        })?;

    let input = reader_thread.join().unwrap()?;
    let output = writer_thread.join().unwrap()?;

    assert_eq!(input.data, 0);
    assert_eq!(output.data, DATA_SIZE);

    Ok(())
}

struct DummyInput<R: Rng> {
    rng: R,
    data: usize,
}

impl<R: Rng> io::Read for DummyInput<R> {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        unimplemented!("unvectored read")
    }

    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        let len = bufs.iter().map(|b| b.len()).sum::<usize>().min(self.data);
        let n = if len <= 10 {
            len
        } else {
            self.rng.random_range(1..len)
        };
        self.data -= n;
        Ok(n)
    }
}

struct DummyOutput<R: Rng> {
    rng: R,
    data: usize,
}

impl<R: Rng> io::Write for DummyOutput<R> {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        unimplemented!("unvectored write")
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        let len = bufs.iter().map(|b| b.len()).sum::<usize>();
        let n = if len <= 10 {
            len
        } else {
            self.rng.random_range(0..len)
        };
        self.data += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
