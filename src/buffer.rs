use crate::parsing::ParsingContext;
use amq_protocol::frame::{BackToTheBuffer, GenError, GenResult, WriteContext};
use std::{
    cmp,
    io::{self, IoSlice, IoSliceMut},
};

#[derive(Debug, PartialEq, Clone)]
pub(crate) struct Buffer {
    memory: Vec<u8>,
    capacity: usize,
    position: usize,
    end: usize,
    available_data: usize,
}

// The bool is true when the checkpoint is to go backwards, false for forward
pub(crate) struct Checkpoint(usize, bool);

impl Buffer {
    pub(crate) fn with_capacity(capacity: usize) -> Buffer {
        Buffer {
            memory: vec![0; capacity],
            capacity,
            position: 0,
            end: 0,
            available_data: 0,
        }
    }

    pub(crate) fn checkpoint(&self) -> Checkpoint {
        Checkpoint(self.end, true)
    }

    pub(crate) fn rollback(&mut self, checkpoint: Checkpoint) {
        let diff = if self.end > checkpoint.0 {
            self.end - checkpoint.0
        } else {
            checkpoint.0 - self.end
        };
        if checkpoint.1 {
            self.available_data -= diff;
        } else {
            self.available_data += diff;
        }
        self.end = checkpoint.0;
    }

    pub(crate) fn grow(&mut self, new_size: usize) -> bool {
        if self.capacity >= new_size {
            return false;
        }

        let old_capacity = self.capacity;

        self.memory.resize(new_size, 0);
        self.capacity = new_size;

        if self.end < self.position {
            // FIXME: make sure there's enough room
            let (old, new) = self.memory.split_at_mut(old_capacity);
            new[..].copy_from_slice(&old[..self.end]);
            self.end += old_capacity;
        }

        true
    }

    pub(crate) fn available_data(&self) -> usize {
        self.available_data
    }

    fn available_space(&self) -> usize {
        self.capacity - self.available_data
    }

    pub(crate) fn consume(&mut self, count: usize) -> usize {
        let cnt = cmp::min(count, self.available_data());
        self.position += cnt;
        self.position %= self.capacity;
        self.available_data -= cnt;
        cnt
    }

    pub(crate) fn fill(&mut self, count: usize) -> usize {
        let cnt = cmp::min(count, self.available_space());
        self.end += cnt;
        self.end %= self.capacity;
        self.available_data += cnt;
        cnt
    }

    pub(crate) fn write_to<T: io::Write>(&self, writer: &mut T) -> io::Result<usize> {
        if self.available_data() == 0 {
            Ok(0)
        } else {
            if self.end > self.position {
                writer.write(&self.memory[self.position..self.end])
            } else {
                writer.write_vectored(&[
                    IoSlice::new(&self.memory[self.position..]),
                    IoSlice::new(&self.memory[..self.end]),
                ])
            }
        }
    }

    pub(crate) fn read_from<T: io::Read>(&mut self, reader: &mut T) -> io::Result<usize> {
        if self.available_space() == 0 {
            Ok(0)
        } else {
            if self.end >= self.position {
                let (start, end) = self.memory.split_at_mut(self.end);
                reader.read_vectored(
                    &mut [
                        IoSliceMut::new(&mut end[..]),
                        IoSliceMut::new(&mut start[..self.position]),
                    ][..],
                )
            } else {
                reader.read(&mut self.memory[self.end..self.position])
            }
        }
    }

    pub(crate) fn offset(&self, buf: ParsingContext<'_>) -> usize {
        let bufptr = buf.as_ptr() as usize;
        if self.end >= self.position {
            let data = &self.memory[self.position..self.end];
            let dataptr = data.as_ptr() as usize;
            bufptr - dataptr
        } else {
            let data = &self.memory[self.position..];
            let dataptr = data.as_ptr() as usize;
            if dataptr < bufptr {
                bufptr - dataptr
            } else {
                let data = &self.memory[..self.end];
                let dataptr = data.as_ptr() as usize;
                bufptr + self.capacity - self.position - dataptr
            }
        }
    }

    pub(crate) fn parsing_context(&self) -> ParsingContext<'_> {
        if self.available_data() == 0 {
            self.memory[self.end..].into()
        } else {
            if self.end > self.position {
                self.memory[self.position..self.end].into()
            } else {
                [&self.memory[self.position..], &self.memory[..self.end]].into()
            }
        }
    }
}

impl io::Write for &mut Buffer {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let amt = if self.available_space() == 0 {
            0
        } else {
            if self.end >= self.position {
                let mut space = &mut self.memory[self.end..];
                let mut amt = space.write(data)?;
                if amt == self.capacity - self.end {
                    let mut space = &mut self.memory[..self.position];
                    amt += space.write(&data[amt..])?;
                }
                amt
            } else {
                let mut space = &mut self.memory[self.end..self.position];
                space.write(data)?
            }
        };
        self.fill(amt);
        Ok(amt)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl BackToTheBuffer for &mut Buffer {
    fn reserve_write_use<
        Tmp,
        Gen: Fn(WriteContext<Self>) -> Result<(WriteContext<Self>, Tmp), GenError>,
        Before: Fn(WriteContext<Self>, Tmp) -> GenResult<Self>,
    >(
        s: WriteContext<Self>,
        reserved: usize,
        gen: &Gen,
        before: &Before,
    ) -> Result<WriteContext<Self>, GenError> {
        if s.write.available_space() < reserved {
            return Err(GenError::BufferTooSmall(
                reserved - s.write.available_space(),
            ));
        }
        let start = s.write.checkpoint();
        s.write.fill(reserved);
        gen(s).and_then(|(s, tmp)| {
            let mut end = s.write.checkpoint();
            end.1 = false;
            s.write.rollback(start);
            before(s, tmp).and_then(|s| {
                s.write.rollback(end);
                Ok(s)
            })
        })
    }
}
