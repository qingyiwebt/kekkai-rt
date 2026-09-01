use anyhow::{anyhow, bail, Context};
use std::{ffi::OsString, os::unix::ffi::OsStringExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;
pub const MAX_FIELD_BYTES: u32 = 1024 * 1024;
pub const MAX_ARGUMENTS: u32 = 1024;
pub const TOOL_START_FAILURE: i32 = 127;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameKind {
    Open,
    Stdin,
    StdinEof,
    Stdout,
    Stderr,
    Exit,
    Error,
    Unknown(u8),
}

impl FrameKind {
    fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::Open,
            2 => Self::Stdin,
            3 => Self::StdinEof,
            0x10 => Self::Stdout,
            0x11 => Self::Stderr,
            0x12 => Self::Exit,
            0x13 => Self::Error,
            value => Self::Unknown(value),
        }
    }
    fn wire(self) -> u8 {
        match self {
            Self::Open => 1,
            Self::Stdin => 2,
            Self::StdinEof => 3,
            Self::Stdout => 0x10,
            Self::Stderr => 0x11,
            Self::Exit => 0x12,
            Self::Error => 0x13,
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Debug)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}
#[derive(Debug)]
pub struct OpenRequest {
    pub command: String,
    pub args: Vec<OsString>,
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Frame> {
    let kind = FrameKind::from_wire(reader.read_u8().await?);
    let length = reader.read_u32().await?;
    if length > MAX_FRAME_BYTES {
        bail!("frame length exceeds {MAX_FRAME_BYTES}");
    }
    let mut payload = vec![0; length as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Frame { kind, payload })
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: FrameKind,
    payload: &[u8],
) -> anyhow::Result<()> {
    if payload.len() > MAX_FRAME_BYTES as usize {
        bail!("frame length exceeds {MAX_FRAME_BYTES}");
    }
    writer.write_u8(kind.wire()).await?;
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

pub fn parse_open(payload: &[u8]) -> anyhow::Result<OpenRequest> {
    let mut input = PayloadReader::new(payload);
    let command = String::from_utf8(input.field()?).context("tool command is not valid UTF-8")?;
    if command.is_empty() {
        bail!("tool command must not be empty");
    }
    let argc = input.u32()?;
    if argc > MAX_ARGUMENTS {
        bail!("argument count exceeds {MAX_ARGUMENTS}");
    }
    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        args.push(OsString::from_vec(input.field()?));
    }
    input.finish()?;
    Ok(OpenRequest { command, args })
}

struct PayloadReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> PayloadReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("four-byte slice"),
        ))
    }
    fn field(&mut self) -> anyhow::Result<Vec<u8>> {
        let length = self.u32()?;
        if length > MAX_FIELD_BYTES {
            bail!("field length exceeds {MAX_FIELD_BYTES}");
        }
        Ok(self.take(length as usize)?.to_vec())
    }
    fn take(&mut self, length: usize) -> anyhow::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| anyhow!("frame length overflow"))?;
        if end > self.bytes.len() {
            bail!("truncated frame payload");
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }
    fn finish(self) -> anyhow::Result<()> {
        if self.offset != self.bytes.len() {
            bail!("unexpected trailing bytes in OPEN frame");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    fn open_payload(command: &[u8], args: &[&[u8]]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&(command.len() as u32).to_be_bytes());
        payload.extend_from_slice(command);
        payload.extend_from_slice(&(args.len() as u32).to_be_bytes());
        for arg in args {
            payload.extend_from_slice(&(arg.len() as u32).to_be_bytes());
            payload.extend_from_slice(arg);
        }
        payload
    }
    #[test]
    fn parses_open_request_with_binary_arguments() {
        let request = parse_open(&open_payload(b"tool", &[b"a", b"\0b"])).unwrap();
        assert_eq!(request.command, "tool");
        assert_eq!(request.args[1].as_encoded_bytes(), b"\0b");
    }
    #[test]
    fn rejects_invalid_open_boundaries() {
        assert!(parse_open(&[]).is_err());
        let mut trailing = open_payload(b"x", &[]);
        trailing.push(1);
        assert!(parse_open(&trailing).is_err());
        let mut too_many = open_payload(b"x", &[]);
        too_many.truncate(5);
        too_many.extend_from_slice(&(MAX_ARGUMENTS + 1).to_be_bytes());
        assert!(parse_open(&too_many).is_err());
    }
    #[tokio::test]
    async fn reads_unknown_and_rejects_oversized_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_all(&[0xfe, 0, 0, 0, 0]).await.unwrap();
        assert_eq!(
            read_frame(&mut reader).await.unwrap().kind,
            FrameKind::Unknown(0xfe)
        );
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_all(&[1]).await.unwrap();
        writer
            .write_all(&(MAX_FRAME_BYTES + 1).to_be_bytes())
            .await
            .unwrap();
        assert!(read_frame(&mut reader).await.is_err());
    }
    #[tokio::test]
    async fn writes_exit_as_a_signed_i32_payload() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        write_frame(&mut writer, FrameKind::Exit, &(-1_i32).to_be_bytes())
            .await
            .unwrap();
        let frame = read_frame(&mut reader).await.unwrap();
        assert_eq!(frame.kind, FrameKind::Exit);
        assert_eq!(i32::from_be_bytes(frame.payload.try_into().unwrap()), -1);
    }
}
