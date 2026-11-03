use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use wasmer_wasix::virtual_fs::{AsyncRead, AsyncSeek, AsyncWrite, FsError, Result, VirtualFile};

use crate::types::{HostDeviceStart, WorkerOut};

pub const HOST_DEVICE_PATH: &str = "/dev/debugger-sh-host";
const CAPACITY: u32 = 64 * 1024;

/// A single occupied slot: 0 = empty, -1 = closed, otherwise its byte length.
#[derive(Debug)]
struct Mailbox {
    length: js_sys::Int32Array,
    bytes: js_sys::Uint8Array,
}

unsafe impl Send for Mailbox {}
unsafe impl Sync for Mailbox {}

impl Mailbox {
    fn new(buffer: &js_sys::SharedArrayBuffer) -> Result<Self> {
        if buffer.byte_length() != 4 + CAPACITY {
            return Err(FsError::InvalidInput);
        }
        Ok(Self {
            length: js_sys::Int32Array::new_with_byte_offset_and_length(buffer, 0, 1),
            bytes: js_sys::Uint8Array::new_with_byte_offset(buffer, 4),
        })
    }

    fn load(&self) -> io::Result<i32> {
        js_sys::Atomics::load(&self.length, 0).map_err(|_| io::ErrorKind::Other.into())
    }

    fn publish(&self, expected: i32, length: i32) -> io::Result<()> {
        // A concurrent close must never be overwritten by a reader or writer.
        if js_sys::Atomics::compare_exchange(&self.length, 0, expected, length)
            .map_err(|_| io::Error::from(io::ErrorKind::Other))?
            != expected
        {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        js_sys::Atomics::notify(&self.length, 0)
            .map_err(|_| io::Error::from(io::ErrorKind::Other))?;
        Ok(())
    }

    fn wait(&self, observed: i32) -> io::Result<()> {
        js_sys::Atomics::wait(&self.length, 0, observed)
            .map(|_| ())
            .map_err(|_| io::ErrorKind::Other.into())
    }
}

#[derive(Debug)]
pub struct HostDeviceFile {
    incoming: Mailbox,
    outgoing: Mailbox,
    read_offset: u32,
}

impl HostDeviceFile {
    pub fn new(start: HostDeviceStart) -> Result<Self> {
        if js_sys::Object::is(&start.guest_to_host, &start.host_to_guest) {
            return Err(FsError::InvalidInput);
        }
        Ok(Self {
            incoming: Mailbox::new(&start.host_to_guest)?,
            outgoing: Mailbox::new(&start.guest_to_host)?,
            read_offset: 0,
        })
    }
}

impl AsyncRead for HostDeviceFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut wasmer_wasix::virtual_fs::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            let length = self.incoming.load()?;
            if length < 0 {
                return Poll::Ready(Ok(()));
            }
            if length == 0 {
                self.incoming.wait(length)?;
                continue;
            }
            let count = (length as u32 - self.read_offset).min(buf.remaining() as u32);
            buf.put_slice(
                &self
                    .incoming
                    .bytes
                    .slice(self.read_offset, self.read_offset + count)
                    .to_vec(),
            );
            self.read_offset += count;
            if self.read_offset == length as u32 {
                self.read_offset = 0;
                self.incoming.publish(length, 0)?;
            }
            return Poll::Ready(Ok(()));
        }
    }
}

impl AsyncWrite for HostDeviceFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            let length = self.outgoing.load()?;
            if length < 0 {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            if length != 0 {
                self.outgoing.wait(length)?;
                continue;
            }
            let count = buf.len().min(CAPACITY as usize);
            self.outgoing
                .bytes
                .set(&js_sys::Uint8Array::from(&buf[..count]), 0);
            self.outgoing.publish(0, count as i32)?;
            // One occupied slot bounds pending wake messages without coalescing.
            WorkerOut::HostWake.send();
            return Poll::Ready(Ok(count));
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for HostDeviceFile {
    fn start_seek(self: Pin<&mut Self>, _position: io::SeekFrom) -> io::Result<()> {
        // WASI seeks non-stdio handles before I/O; this device is a byte stream.
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

impl VirtualFile for HostDeviceFile {
    fn last_accessed(&self) -> u64 {
        0
    }
    fn last_modified(&self) -> u64 {
        0
    }
    fn created_time(&self) -> u64 {
        0
    }
    fn size(&self) -> u64 {
        0
    }
    fn set_len(&mut self, _new_size: u64) -> Result<()> {
        Err(FsError::Unsupported)
    }
    fn unlink(&mut self) -> Result<()> {
        Err(FsError::Unsupported)
    }
    fn poll_read_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }
    fn poll_write_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::ErrorKind::Unsupported.into()))
    }
}
