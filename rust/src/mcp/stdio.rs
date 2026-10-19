use std::fmt;

use serde_json::Value;

use super::IncomingMessage;

pub const DEFAULT_STDIO_BUFFER_LIMIT: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdioError {
    BufferLimit(usize),
    InvalidJson(String),
    InvalidMessage,
}

impl fmt::Display for StdioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferLimit(limit) => {
                write!(f, "ReadBuffer exceeded maximum size of {limit} bytes")
            }
            Self::InvalidJson(message) => f.write_str(message),
            Self::InvalidMessage => f.write_str("Invalid JSON-RPC message"),
        }
    }
}

impl std::error::Error for StdioError {}

#[derive(Debug)]
pub struct StdioCodec {
    buffer: Vec<u8>,
    max_buffer_bytes: usize,
}

impl Default for StdioCodec {
    fn default() -> Self {
        Self::new(DEFAULT_STDIO_BUFFER_LIMIT)
    }
}

impl StdioCodec {
    pub fn new(max_buffer_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_buffer_bytes,
        }
    }

    pub fn append(&mut self, bytes: &[u8]) -> Result<(), StdioError> {
        if bytes.len() > self.max_buffer_bytes.saturating_sub(self.buffer.len()) {
            self.buffer.clear();
            return Err(StdioError::BufferLimit(self.max_buffer_bytes));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    pub fn read_message(&mut self) -> Result<Option<IncomingMessage>, StdioError> {
        let Some(index) = self.buffer.iter().position(|b| *b == b'\n') else {
            return Ok(None);
        };
        let line = String::from_utf8_lossy(&self.buffer[..index]).into_owned();
        self.buffer.drain(..=index);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        let value =
            serde_json::from_str(line).map_err(|e| StdioError::InvalidJson(e.to_string()))?;
        IncomingMessage::parse(value)
            .map(Some)
            .map_err(|_| StdioError::InvalidMessage)
    }

    pub fn encode(message: &Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(message).expect("JSON message");
        bytes.push(b'\n');
        bytes
    }
}

#[cfg(unix)]
pub(crate) struct FrameWriter {
    descriptor: std::os::fd::OwnedFd,
    original_flags: libc::c_int,
    readiness: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
}

#[cfg(unix)]
impl FrameWriter {
    pub(crate) fn new() -> std::io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        // dup shares stdout status flags. Stdio must be its sole writer and flag owner
        // until Drop restores them. A concurrent flag change would also be overwritten.
        let duplicate = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let descriptor = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicate) };
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut writer = Self {
            descriptor,
            original_flags: flags,
            readiness: None,
        };
        if unsafe {
            libc::fcntl(
                writer.descriptor.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let readiness_fd = writer.descriptor.try_clone()?;
        // OwnedFd keeps the registered descriptor open and stable until AsyncFd drops it.
        writer.readiness = match unsafe { tokio::io::unix::AsyncFd::register(readiness_fd) }
            .map_err(std::io::Error::from)
        {
            Ok(readiness) => Some(readiness),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => None,
            Err(error) => return Err(error),
        };
        Ok(writer)
    }

    pub(crate) async fn write(&mut self, frame: &[u8]) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut remaining = frame;
        while !remaining.is_empty() {
            let write = || {
                let count = unsafe {
                    libc::write(
                        self.descriptor.as_raw_fd(),
                        remaining.as_ptr().cast(),
                        remaining.len(),
                    )
                };
                if count < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(count as usize)
                }
            };
            let count = if let Some(readiness) = &self.readiness {
                let mut ready = readiness.writable().await?;
                match ready.try_io(|_| write()) {
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(result) => result?,
                    Err(_) => continue,
                }
            } else {
                write()?
            };
            if count == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            remaining = &remaining[count..];
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for FrameWriter {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        loop {
            if unsafe {
                libc::fcntl(
                    self.descriptor.as_raw_fd(),
                    libc::F_SETFL,
                    self.original_flags,
                )
            } >= 0
            {
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}

#[cfg(not(unix))]
pub(crate) struct FrameWriter(tokio::io::Stdout);

#[cfg(not(unix))]
impl FrameWriter {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self(tokio::io::stdout()))
    }
    pub(crate) async fn write(&mut self, frame: &[u8]) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.0.write_all(frame).await?;
        self.0.flush().await
    }
}
