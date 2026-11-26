use std::io::{Read, Write};

use bincode::Options;
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::ProtoError;

pub const HEADER_LEN: usize = 6;
pub const MAX_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;
pub const MAX_DECOMPRESSED_PAYLOAD: usize = 32 * 1024 * 1024;

pub const FLAG_NONE: u8 = 0x00;
pub const FLAG_COMPRESSED: u8 = 0x01;
pub const FLAG_KEYFRAME: u8 = 0x02;
pub const FLAG_END_OF_STREAM: u8 = 0x04;
pub const FLAG_URGENT: u8 = 0x08;
const KNOWN_FLAGS_MASK: u8 = FLAG_COMPRESSED | FLAG_KEYFRAME | FLAG_END_OF_STREAM | FLAG_URGENT;

/// Logical multiplexed channel identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChannelId {
    AuthControl = 0x01,
    VideoTiles = 0x02,
    Input = 0x03,
    Clipboard = 0x04,
    FileTransfer = 0x05,
    Terminal = 0x06,
    Tunnel = 0x07,
    SysAdmin = 0x08,
    Comms = 0x09,
    PowerControl = 0x0A,
}

impl ChannelId {
    /// Returns channel scheduling priority (lower number = higher priority).
    pub const fn priority(self) -> u8 {
        match self {
            Self::Input => 0,
            Self::AuthControl | Self::Terminal | Self::Comms | Self::PowerControl => 1,
            Self::Clipboard | Self::Tunnel | Self::SysAdmin => 2,
            Self::VideoTiles => 3,
            Self::FileTransfer => 4,
        }
    }
}

impl TryFrom<u8> for ChannelId {
    type Error = ProtoError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::AuthControl),
            0x02 => Ok(Self::VideoTiles),
            0x03 => Ok(Self::Input),
            0x04 => Ok(Self::Clipboard),
            0x05 => Ok(Self::FileTransfer),
            0x06 => Ok(Self::Terminal),
            0x07 => Ok(Self::Tunnel),
            0x08 => Ok(Self::SysAdmin),
            0x09 => Ok(Self::Comms),
            0x0A => Ok(Self::PowerControl),
            other => Err(ProtoError::UnknownChannel(other)),
        }
    }
}

/// Raw wire frame `[u32 BE length][u8 channel_id][u8 flags][payload]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub channel: ChannelId,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Validates channel, flags, and payload bounds prior to transmission.
    pub fn new(channel: ChannelId, flags: u8, payload: Vec<u8>) -> Result<Self, ProtoError> {
        validate_header(payload.len(), flags)?;
        Ok(Self {
            channel,
            flags,
            payload,
        })
    }
}

fn validate_header(len: usize, flags: u8) -> Result<(), ProtoError> {
    if len > MAX_FRAME_PAYLOAD {
        return Err(ProtoError::FrameTooLarge {
            len,
            max: MAX_FRAME_PAYLOAD,
        });
    }
    if flags & !KNOWN_FLAGS_MASK != 0 {
        return Err(ProtoError::InvalidFrameFlags(flags));
    }
    Ok(())
}

/// Deterministic `bincode 1.3` options enforcing trailing-byte rejection and allocation caps.
pub fn bincode_options() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .with_limit(MAX_DECOMPRESSED_PAYLOAD as u64)
}

/// Serializes a typed protocol payload using bounded `bincode`.
pub fn encode_bincode<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    bincode_options()
        .serialize(value)
        .map_err(|e| ProtoError::Codec(format!("bincode serialize: {e}")))
}

/// Deserializes a typed protocol payload using bounded `bincode`.
pub fn decode_bincode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    bincode_options()
        .deserialize(bytes)
        .map_err(|e| ProtoError::Codec(format!("bincode deserialize: {e}")))
}

/// Serializes a message into a `Frame`, optionally compressing large payloads with Zstd.
pub fn pack_message<T: Serialize>(
    channel: ChannelId,
    mut flags: u8,
    value: &T,
    compress_threshold: Option<usize>,
) -> Result<Frame, ProtoError> {
    let raw = encode_bincode(value)?;
    let payload = match compress_threshold {
        Some(limit) if raw.len() >= limit => {
            flags |= FLAG_COMPRESSED;
            zstd::stream::encode_all(raw.as_slice(), 3)
                .map_err(|e| ProtoError::Codec(format!("zstd compress: {e}")))?
        }
        _ => raw,
    };
    Frame::new(channel, flags, payload)
}

/// Unpacks a `Frame`, performing bounded Zstd decompression if `FLAG_COMPRESSED` is set.
pub fn unpack_message<T: DeserializeOwned>(frame: &Frame) -> Result<T, ProtoError> {
    if frame.flags & FLAG_COMPRESSED != 0 {
        let decoder = zstd::stream::Decoder::new(frame.payload.as_slice())
            .map_err(|e| ProtoError::Codec(format!("zstd decoder init: {e}")))?;
        let mut bounded = decoder.take(MAX_DECOMPRESSED_PAYLOAD as u64 + 1);
        let mut raw = Vec::new();
        bounded
            .read_to_end(&mut raw)
            .map_err(|e| ProtoError::Codec(format!("zstd decompress: {e}")))?;
        if raw.len() > MAX_DECOMPRESSED_PAYLOAD {
            return Err(ProtoError::DecompressedTooLarge {
                len: raw.len(),
                max: MAX_DECOMPRESSED_PAYLOAD,
            });
        }
        decode_bincode(&raw)
    } else {
        decode_bincode(&frame.payload)
    }
}

fn build_header(len: usize, channel: ChannelId, flags: u8) -> Result<[u8; HEADER_LEN], ProtoError> {
    validate_header(len, flags)?;
    let len_bytes = (len as u32).to_be_bytes();
    Ok([
        len_bytes[0],
        len_bytes[1],
        len_bytes[2],
        len_bytes[3],
        channel as u8,
        flags,
    ])
}

fn parse_header(header: [u8; HEADER_LEN]) -> Result<(usize, ChannelId, u8), ProtoError> {
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let channel = ChannelId::try_from(header[4])?;
    let flags = header[5];
    validate_header(len, flags)?;
    Ok((len, channel, flags))
}

/// Writes a framed packet to an async stream.
pub async fn write_frame_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    channel: ChannelId,
    flags: u8,
    payload: &[u8],
) -> Result<(), ProtoError> {
    let header = build_header(payload.len(), channel, flags)?;
    writer.write_all(&header).await?;
    if !payload.is_empty() {
        writer.write_all(payload).await?;
    }
    writer.flush().await?;
    Ok(())
}

/// Reads the next framed packet from an async stream; returns `Ok(None)` on clean EOF at boundary.
pub async fn read_frame_async<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Frame>, ProtoError> {
    AsyncFrameReader::default().read(reader).await
}

/// Retains partial headers and payloads when another branch wins a session's select.
#[derive(Default)]
pub struct AsyncFrameReader {
    header: [u8; HEADER_LEN],
    header_read: usize,
    payload: Option<(Frame, usize)>,
}

impl AsyncFrameReader {
    pub async fn read<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<Frame>, ProtoError> {
        while self.header_read < HEADER_LEN {
            let n = reader.read(&mut self.header[self.header_read..]).await?;
            if n == 0 && self.header_read == 0 {
                return Ok(None);
            }
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.header_read += n;
        }
        if self.payload.is_none() {
            let (len, channel, flags) = parse_header(self.header)?;
            self.payload = Some((Frame { channel, flags, payload: vec![0; len] }, 0));
        }
        let (frame, bytes_read) = self.payload.as_mut().unwrap();
        while *bytes_read < frame.payload.len() {
            let n = reader.read(&mut frame.payload[*bytes_read..]).await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            *bytes_read += n;
        }
        self.header_read = 0;
        Ok(self.payload.take().map(|(frame, _)| frame))
    }
}

/// Writes a framed packet to a synchronous stream (useful for Win32 named-pipe helpers).
pub fn write_frame_sync<W: Write>(
    writer: &mut W,
    channel: ChannelId,
    flags: u8,
    payload: &[u8],
) -> Result<(), ProtoError> {
    let header = build_header(payload.len(), channel, flags)?;
    writer.write_all(&header)?;
    if !payload.is_empty() {
        writer.write_all(payload)?;
    }
    writer.flush()?;
    Ok(())
}

/// Reads a framed packet from a synchronous stream; returns `Ok(None)` on clean EOF at boundary.
pub fn read_frame_sync<R: Read>(reader: &mut R) -> Result<Option<Frame>, ProtoError> {
    let mut header = [0u8; HEADER_LEN];
    let first = reader.read(&mut header[..1])?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;
    let (len, channel, flags) = parse_header(header)?;
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload)?;
    }
    Ok(Some(Frame {
        channel,
        flags,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;
    use crate::messages::{ControlMsg, SessionMode};

    #[tokio::test]
    async fn async_reader_retains_partial_frame_across_cancellation() {
        let (mut tx, mut rx) = duplex(32);
        let mut reader = AsyncFrameReader::default();
        let header = build_header(4, ChannelId::Clipboard, FLAG_NONE).unwrap();
        let wait = std::time::Duration::from_millis(5);
        tx.write_all(&header[..2]).await.unwrap();
        assert!(tokio::time::timeout(wait, reader.read(&mut rx)).await.is_err());
        tx.write_all(&header[2..]).await.unwrap();
        tx.write_all(b"ab").await.unwrap();
        assert!(tokio::time::timeout(wait, reader.read(&mut rx)).await.is_err());
        tx.write_all(b"cd").await.unwrap();
        assert_eq!(reader.read(&mut rx).await.unwrap().unwrap().payload, b"abcd");
        drop(tx);
        assert!(reader.read(&mut rx).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn frame_roundtrip_and_zero_length_eof() {
        let (mut tx, mut rx) = duplex(1024);
        write_frame_async(&mut tx, ChannelId::AuthControl, FLAG_URGENT, &[])
            .await
            .unwrap();
        write_frame_async(&mut tx, ChannelId::Input, FLAG_NONE, b"ping")
            .await
            .unwrap();
        drop(tx);

        let f1 = read_frame_async(&mut rx).await.unwrap().unwrap();
        assert_eq!(f1.channel, ChannelId::AuthControl);
        assert_eq!(f1.flags, FLAG_URGENT);
        assert!(f1.payload.is_empty());

        let f2 = read_frame_async(&mut rx).await.unwrap().unwrap();
        assert_eq!(f2.channel, ChannelId::Input);
        assert_eq!(f2.payload, b"ping");

        assert!(read_frame_async(&mut rx).await.unwrap().is_none());

        for (raw, expected) in [
            (0x06, ChannelId::Terminal),
            (0x07, ChannelId::Tunnel),
            (0x08, ChannelId::SysAdmin),
            (0x09, ChannelId::Comms),
            (0x0A, ChannelId::PowerControl),
        ] {
            assert_eq!(ChannelId::try_from(raw).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn oversize_and_unknown_channel_rejected() {
        let (mut tx, mut rx) = duplex(64);
        let bad_len = ((MAX_FRAME_PAYLOAD as u32) + 1).to_be_bytes();
        tokio::io::AsyncWriteExt::write_all(
            &mut tx,
            &[bad_len[0], bad_len[1], bad_len[2], bad_len[3], 0x01, 0x00],
        )
        .await
        .unwrap();

        let err = read_frame_async(&mut rx).await.unwrap_err();
        assert!(matches!(err, ProtoError::FrameTooLarge { .. }));

        let (mut tx2, mut rx2) = duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut tx2, &[0, 0, 0, 0, 0x99, 0x00])
            .await
            .unwrap();
        let err2 = read_frame_async(&mut rx2).await.unwrap_err();
        assert!(matches!(err2, ProtoError::UnknownChannel(0x99)));
    }

    #[test]
    fn compressed_pack_unpack_and_zstd_bomb_guard() {
        let msg = ControlMsg::Disconnect {
            reason: "session closed ".repeat(200),
        };
        let frame = pack_message(ChannelId::AuthControl, FLAG_NONE, &msg, Some(64)).unwrap();
        assert_ne!(frame.flags & FLAG_COMPRESSED, 0);
        assert!(frame.payload.len() < 200);
        let decoded: ControlMsg = unpack_message(&frame).unwrap();
        assert_eq!(decoded, msg);

        // Verify decompression cap blocks a >32 MiB compressed stream without allocating >32 MiB
        let huge = vec![b'A'; MAX_DECOMPRESSED_PAYLOAD + 1024];
        let compressed = zstd::stream::encode_all(huge.as_slice(), 1).unwrap();
        let bomb_frame = Frame::new(ChannelId::AuthControl, FLAG_COMPRESSED, compressed).unwrap();
        let err = unpack_message::<ControlMsg>(&bomb_frame).unwrap_err();
        assert!(matches!(err, ProtoError::DecompressedTooLarge { .. }));
    }

    #[test]
    fn bincode_rejects_trailing_bytes() {
        let msg = ControlMsg::SessionModeSwitch {
            mode: SessionMode::ViewOnly,
        };
        let mut bytes = encode_bincode(&msg).unwrap();
        bytes.push(0xAA);
        assert!(decode_bincode::<ControlMsg>(&bytes).is_err());
    }
}
