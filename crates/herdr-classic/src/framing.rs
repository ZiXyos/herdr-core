use crate::ClassicError;
use crate::protocol::{MAX_FRAME, MAX_GRAPHICS_FRAME, SERVER_PANE_SURFACE_TAG};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Length-prefixes one outbound payload. Writing is owned by the session's outbox.
pub fn frame_bytes(payload: &[u8]) -> Result<Vec<u8>, ClassicError> {
    if payload.len() > MAX_FRAME {
        return Err(ClassicError::new("classic_bound", "outbound frame exceeds 2 MiB"));
    }
    let length = u32::try_from(payload.len()).map_err(|_| ClassicError::new("classic_bound", "frame too large"))?;
    let mut bytes = Vec::with_capacity(4 + payload.len());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

/// Test/handshake convenience. Mutation commands use the persistent `FrameWriter`.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<(), ClassicError> {
    writer.write_all(&frame_bytes(payload)?).await?;
    writer.flush().await?;
    Ok(())
}

/// Persistent, cancellation-safe inbound framing. The only await is one `read`
/// call, whose bytes are appended to `buffer` before the future can be dropped, so
/// dropping `next()` (e.g. in `tokio::select!`) never loses a partial prefix/payload.
pub struct FrameReader<R> {
    read: R,
    buffer: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(read: R) -> Self {
        Self { read, buffer: Vec::new() }
    }

    pub async fn next(&mut self) -> Result<Vec<u8>, ClassicError> {
        loop {
            if let Some(frame) = self.take()? {
                return Ok(frame);
            }
            let mut chunk = [0u8; 16 * 1024];
            let read = self.read.read(&mut chunk).await?;
            if read == 0 {
                return Err(ClassicError::new("classic_transport", "endpoint closed the stream"));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    /// Bounds are checked from the prefix and first payload byte, before the payload
    /// is buffered: only full pane surfaces (tag 13) may use the 32 MiB graphics bound.
    pub(crate) fn take(&mut self) -> Result<Option<Vec<u8>>, ClassicError> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_le_bytes(self.buffer[..4].try_into().expect("four bytes")) as usize;
        if length > MAX_GRAPHICS_FRAME {
            return Err(ClassicError::new("classic_bound", "inbound frame exceeds 32 MiB"));
        }
        if length > MAX_FRAME {
            let Some(&tag) = self.buffer.get(4) else { return Ok(None) };
            if u32::from(tag) != SERVER_PANE_SURFACE_TAG {
                return Err(ClassicError::new("classic_bound", "non-surface inbound frame exceeds 2 MiB"));
            }
        }
        if self.buffer.len() < 4 + length {
            return Ok(None);
        }
        let frame = self.buffer[4..4 + length].to_vec();
        self.buffer.drain(..4 + length);
        Ok(Some(frame))
    }
}

/// A single persistent outbound frame. Cancelling `flush` retains both bytes and
/// progress. Once any bytes may have reached the server, an I/O failure is fatal and
/// the mutation outcome is unknown; callers must reconnect and inspect authoritative state.
pub struct FrameWriter<W> {
    write: W,
    frame: Vec<u8>,
    written: usize,
}

impl<W> FrameWriter<W> {
    pub fn new(write: W) -> Self {
        Self { write, frame: Vec::new(), written: 0 }
    }

    pub fn is_pending(&self) -> bool {
        !self.frame.is_empty()
    }

    pub fn has_written(&self) -> bool {
        self.written > 0
    }

    /// Retires a staged frame of which no byte was transmitted. A partially written
    /// frame can never be discarded: truncating it would desynchronize the stream.
    pub fn discard_unsent(&mut self) {
        assert!(!self.has_written(), "cannot discard a partially transmitted frame");
        self.frame.clear();
    }
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Best-effort transport close after a fatal error (e.g. a stale partial frame).
    pub async fn shutdown(&mut self) {
        self.frame.clear();
        self.written = 0;
        let _ = self.write.shutdown().await;
    }

    pub fn stage(&mut self, payload: &[u8]) -> Result<(), ClassicError> {
        if !self.frame.is_empty() {
            return Err(ClassicError::protocol("outbound frame staged over an active write"));
        }
        self.frame = frame_bytes(payload)?;
        self.written = 0;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<(), ClassicError> {
        while self.written < self.frame.len() {
            let result = self.write.write(&self.frame[self.written..]).await;
            match result {
                Ok(0) => return Err(self.fail(self.written > 0, "endpoint closed during an outbound frame")),
                Ok(written) => self.written += written,
                Err(error) => return Err(self.fail(self.written > 0, error.to_string())),
            }
        }
        if !self.frame.is_empty()
            && let Err(error) = self.write.flush().await
        {
            return Err(self.fail(true, error.to_string()));
        }
        self.frame.clear();
        self.written = 0;
        Ok(())
    }

    fn fail(&mut self, may_have_written: bool, message: impl Into<String>) -> ClassicError {
        self.frame.clear();
        self.written = 0;
        if may_have_written {
            ClassicError::new(
                "classic_mutation_unknown",
                format!("partial command write; mutation outcome unknown: {}", message.into()),
            )
        } else {
            ClassicError::new("classic_transport", message.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HELLO_KIND, encode_control};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[tokio::test]
    async fn frame_reader_survives_fragmentation_and_cancellation() {
        let payload = encode_control(HELLO_KIND, "hello".into()).unwrap();
        let frame = frame_bytes(&payload).unwrap();
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut reader = FrameReader::new(reader);
        // Deliver half the frame, cancel the pending read, then deliver the rest.
        writer.write_all(&frame[..3]).await.unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), reader.next()).await.is_err());
        writer.write_all(&frame[3..9]).await.unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), reader.next()).await.is_err());
        for byte in &frame[9..] {
            writer.write_all(&[*byte]).await.unwrap();
        }
        assert_eq!(reader.next().await.unwrap(), payload);
    }

    #[tokio::test]
    async fn non_surface_frames_over_two_mib_are_rejected_before_buffering() {
        let large = u32::try_from(MAX_FRAME + 1).unwrap().to_le_bytes();
        // Only prefix + tag are available; the reader must decide without the payload.
        let control = [&large[..], &[20u8][..]].concat();
        let mut reader = FrameReader::new(&control[..]);
        assert_eq!(reader.next().await.unwrap_err().kind, "classic_bound");
        let surface_bytes = [&large[..], &[SERVER_PANE_SURFACE_TAG as u8][..]].concat();
        let mut surface = FrameReader::new(&surface_bytes[..]);
        // A large full surface is allowed to keep buffering (here it then hits EOF).
        assert_eq!(surface.next().await.unwrap_err().kind, "classic_transport");
        let too_large = u32::try_from(MAX_GRAPHICS_FRAME + 1).unwrap().to_le_bytes();
        assert_eq!(FrameReader::new(&too_large[..]).next().await.unwrap_err().kind, "classic_bound");
    }

    #[tokio::test]
    async fn writer_retains_a_cancelled_partial_frame() {
        let payload = vec![7; 4096];
        let (writer, mut reader) = tokio::io::duplex(8);
        let mut writer = FrameWriter::new(writer);
        writer.stage(&payload).unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), writer.flush()).await.is_err());
        assert!(writer.is_pending() && writer.has_written());
        let task = tokio::spawn(async move {
            let mut bytes = vec![0; 4100];
            reader.read_exact(&mut bytes).await.unwrap();
            bytes
        });
        writer.flush().await.unwrap();
        assert_eq!(task.await.unwrap(), frame_bytes(&payload).unwrap());
    }

    struct FailAfter {
        remaining: usize,
    }

    impl AsyncWrite for FailAfter {
        fn poll_write(mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buffer: &[u8]) -> Poll<std::io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fixture failure")));
            }
            let written = self.remaining.min(buffer.len());
            self.remaining -= written;
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn partial_command_write_is_fatal_with_unknown_outcome() {
        let mut writer = FrameWriter::new(FailAfter { remaining: 8 });
        writer.stage(b"command").unwrap();
        let error = writer.flush().await.unwrap_err();
        assert_eq!(error.kind, "classic_mutation_unknown");
        assert!(!writer.is_pending());
    }
}
