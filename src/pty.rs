// ABOUTME: Phase 1 PTY backend: portable-pty behind the spec's PtyBackend trait.
// ABOUTME: Blocking reads run on a dedicated thread feeding a bounded channel.

use async_trait::async_trait;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use tokio::sync::mpsc;

/// Bounded capacity for the PTY->orchestrator byte channel. Bounding it
/// applies backpressure so a hung renderer cannot exhaust memory.
const OUTPUT_CHANNEL_CAP: usize = 1024;

/// Fixed read buffer per the spec's hot-loop allocation directive.
const READ_BUF: usize = 8192;

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("pty spawn failed: {0}")]
    Spawn(String),
    #[error("pty write failed: {0}")]
    Write(#[from] std::io::Error),
    #[error("pty resize failed: {0}")]
    Resize(String),
}

/// Abstract PTY interface for dependency injection and mocking.
///
/// This is the spec's Phase 1 deliverable surface. The orchestrator drives
/// the PTY through the split halves for a clean ownership graph, so the
/// unified read/write/resize here are exercised by tests/mocks, not main.
#[allow(dead_code)]
#[async_trait]
pub trait PtyBackend {
    type Error: std::error::Error;

    /// Spawns the child process and attaches it to the PTY slave.
    async fn spawn(shell: &str, rows: u16, cols: u16) -> Result<Self, Self::Error>
    where
        Self: Sized;

    /// Reads raw bytes from the PTY master. Should be non-blocking.
    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error>;

    /// Writes raw input bytes to the PTY master.
    async fn write(&mut self, data: &[u8]) -> Result<(), Self::Error>;

    /// Sends an IOCTL TIOCSWINSZ to notify the child of layout changes.
    async fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Self::Error>;
}

/// portable-pty backed implementation.
///
/// `spawn` launches a dedicated OS thread for blocking master reads, which
/// emits chunks into a bounded channel. The unified type satisfies the spec
/// trait for tests; the orchestrator uses [`PortablePty::into_split`] to get
/// independently-borrowable read and control halves (idiomatic over wrapping
/// the whole PTY in a shared lock).
pub struct PortablePty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output_rx: mpsc::Receiver<Vec<u8>>,
}

/// Owns the byte stream coming off the PTY master.
pub struct PtyReader {
    output_rx: mpsc::Receiver<Vec<u8>>,
}

/// Owns write + resize + child lifetime control of the PTY master.
pub struct PtyControl {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

impl PortablePty {
    /// Spawn the child + dedicated read thread. No awaits, so it is shared
    /// by the async trait `spawn` and the sync `spawn_sync`.
    fn build(shell: &str, rows: u16, cols: u16) -> Result<Self, PtyError> {
        let system = native_pty_system();
        let pair = system
            .openpty(pty_size(rows, cols))
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let cmd = CommandBuilder::new(shell);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        // Slave fd is held by the child now; drop our copy so EOF propagates.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;

        let (tx, output_rx) = mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_CAP);

        // Dedicated blocking-read thread (spec Phase 1 concurrency directive).
        // blocking_send needs no runtime; the bound applies backpressure.
        std::thread::spawn(move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self { master: pair.master, writer, child, output_rx })
    }

    /// Synchronous spawn for the winit/GPU loop (no tokio runtime required).
    pub fn spawn_sync(shell: &str, rows: u16, cols: u16) -> Result<Self, PtyError> {
        Self::build(shell, rows, cols)
    }

    /// Split into independently-owned read and control halves.
    pub fn into_split(self) -> (PtyReader, PtyControl) {
        (
            PtyReader { output_rx: self.output_rx },
            PtyControl { master: self.master, writer: self.writer, child: self.child },
        )
    }
}

impl PtyReader {
    /// Await the next chunk from the PTY. Returns None once the child closes.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.output_rx.recv().await
    }

    /// Non-blocking drain for the winit loop.
    pub fn try_recv(&mut self) -> TryChunk {
        match self.output_rx.try_recv() {
            Ok(chunk) => TryChunk::Data(chunk),
            Err(mpsc::error::TryRecvError::Empty) => TryChunk::Empty,
            Err(mpsc::error::TryRecvError::Disconnected) => TryChunk::Closed,
        }
    }
}

/// Result of a non-blocking PTY read for the winit loop.
pub enum TryChunk {
    Data(Vec<u8>),
    Empty,
    Closed,
}

impl PtyControl {
    pub fn write(&mut self, data: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.master
            .resize(pty_size(rows, cols))
            .map_err(|e| PtyError::Resize(e.to_string()))
    }
}

impl Drop for PtyControl {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[async_trait]
impl PtyBackend for PortablePty {
    type Error = PtyError;

    async fn spawn(shell: &str, rows: u16, cols: u16) -> Result<Self, Self::Error> {
        Self::build(shell, rows, cols)
    }

    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error> {
        match self.output_rx.recv().await {
            Some(chunk) => {
                let n = chunk.len().min(buffer.len());
                buffer[..n].copy_from_slice(&chunk[..n]);
                Ok(n)
            }
            None => Ok(0),
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    async fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Self::Error> {
        self.master
            .resize(pty_size(rows, cols))
            .map_err(|e| PtyError::Resize(e.to_string()))
    }
}

// Note: no Drop for PortablePty. The orchestrator always splits it; child
// teardown lives on PtyControl::drop. A Drop here would also block into_split
// from moving fields out (E0509).
