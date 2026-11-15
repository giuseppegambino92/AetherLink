//! Length-prefixed framing: `[u32 LE length][bincode payload]`.

use serde::{Serialize, de::DeserializeOwned};

/// Hard upper bound for a single frame (protects against hostile peers).
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("frame of {0} bytes exceeds the maximum of {MAX_FRAME_LEN}")]
    TooLarge(usize),
    #[error("serialization error: {0}")]
    Codec(#[from] bincode::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("stream closed mid-frame")]
    Truncated,
}

/// Serialize a message to a bare bincode payload (no length prefix) — used for datagrams.
pub fn encode_message<T: Serialize>(msg: &T) -> Result<Vec<u8>, FramingError> {
    let bytes = bincode::serialize(msg)?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(FramingError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Deserialize a bare bincode payload with a size limit.
pub fn decode_message<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FramingError> {
    use bincode::Options;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(FramingError::TooLarge(bytes.len()));
    }
    Ok(bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(MAX_FRAME_LEN as u64)
        .deserialize(bytes)?)
}

/// Serialize a message into a length-prefixed frame.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FramingError> {
    let size = bincode::serialized_size(msg)? as usize;
    if size > MAX_FRAME_LEN {
        return Err(FramingError::TooLarge(size));
    }
    let mut out = Vec::with_capacity(4 + size);
    out.extend_from_slice(&(size as u32).to_le_bytes());
    bincode::serialize_into(&mut out, msg)?;
    Ok(out)
}

/// Incremental decoder for byte streams that may deliver partial frames.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Number of buffered, not yet decoded bytes.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Try to decode the next complete frame.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FramingError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FramingError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = decode_message(&self.buf[4..4 + len])?;
        self.buf.drain(..4 + len);
        Ok(Some(msg))
    }
}

/// Blocking helpers for `std::io` streams.
pub fn write_frame_sync<W: std::io::Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FramingError> {
    w.write_all(&encode_frame(msg)?)?;
    Ok(())
}

/// Blocking read of one frame; `Ok(None)` on clean EOF at a frame boundary.
pub fn read_frame_sync<R: std::io::Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, FramingError> {
    let mut len = [0u8; 4];
    if !read_exact_or_eof(r, &mut len)? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FramingError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof { FramingError::Truncated } else { e.into() }
    })?;
    decode_message(&payload).map(Some)
}

fn read_exact_or_eof<R: std::io::Read>(r: &mut R, buf: &mut [u8]) -> Result<bool, FramingError> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err(FramingError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

#[cfg(feature = "tokio")]
mod async_io {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Write one frame to an async stream (e.g. a `quinn::SendStream`).
    pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FramingError> {
        let frame = encode_frame(msg)?;
        w.write_all(&frame).await?;
        Ok(())
    }

    /// Read one frame from an async stream; `Ok(None)` on clean EOF at a frame boundary.
    pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>, FramingError> {
        let mut len = [0u8; 4];
        let mut filled = 0;
        while filled < 4 {
            let n = r.read(&mut len[filled..]).await?;
            if n == 0 {
                return if filled == 0 { Ok(None) } else { Err(FramingError::Truncated) };
            }
            filled += n;
        }
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_FRAME_LEN {
            return Err(FramingError::TooLarge(len));
        }
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof { FramingError::Truncated } else { e.into() }
        })?;
        decode_message(&payload).map(Some)
    }
}

#[cfg(feature = "tokio")]
pub use async_io::{read_frame, write_frame};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn incremental_decoder_handles_split_frames() {
        let a = ControlMessage::Ping { nonce: 7, sent_us: 99 };
        let b = ControlMessage::RequestKeyframe { display_id: 2 };
        let mut bytes = encode_frame(&a).unwrap();
        bytes.extend(encode_frame(&b).unwrap());

        let mut dec = FrameDecoder::new();
        for chunk in bytes.chunks(3) {
            dec.push(chunk);
        }
        assert_eq!(dec.next_frame::<ControlMessage>().unwrap(), Some(a));
        assert_eq!(dec.next_frame::<ControlMessage>().unwrap(), Some(b));
        assert_eq!(dec.next_frame::<ControlMessage>().unwrap(), None);
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn rejects_oversized_length_prefix() {
        let mut dec = FrameDecoder::new();
        dec.push(&(u32::MAX).to_le_bytes());
        assert!(matches!(dec.next_frame::<ControlMessage>(), Err(FramingError::TooLarge(_))));
    }

    #[test]
    fn sync_roundtrip_and_eof() {
        let msgs = vec![
            InputEvent::Key { code: HidUsage::A, pressed: true, modifiers: Modifiers::SHIFT },
            InputEvent::Text { text: "ciao 👋".into() },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_frame_sync(&mut buf, m).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        let mut out = Vec::new();
        while let Some(m) = read_frame_sync::<_, InputEvent>(&mut cur).unwrap() {
            out.push(m);
        }
        assert_eq!(out, msgs);
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let mut bytes = encode_frame(&ControlMessage::Bye).unwrap();
        bytes.extend_from_slice(&[10, 0, 0, 0, 1]);
        let mut cur = std::io::Cursor::new(bytes);
        assert!(read_frame_sync::<_, ControlMessage>(&mut cur).unwrap().is_some());
        assert!(matches!(read_frame_sync::<_, ControlMessage>(&mut cur), Err(FramingError::Truncated)));
    }

    #[tokio::test]
    async fn async_roundtrip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let frame = VideoMessage::Frame(VideoFrame {
            display_id: 1,
            seq: 42,
            pts_us: 1_000,
            codec: VideoCodec::Hevc,
            chroma: ChromaFormat::Yuv444,
            keyframe: true,
            width: 3840,
            height: 2160,
            data: vec![0xAB; 10_000],
        });
        let expected = frame.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut a, &frame).await.unwrap();
        });
        let got: VideoMessage = read_frame(&mut b).await.unwrap().unwrap();
        writer.await.unwrap();
        assert_eq!(got, expected);
    }
}
