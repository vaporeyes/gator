## Phase 1: PTY Management & Core Event Loop

**Objective:** Define the asynchronous boundaries between the host operating system, the child shell process, and the internal application state.

**Data Structures & Interfaces:**

```rust
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Represents discrete state changes injected into the event loop.
pub enum TerminalEvent {
    /// Bytes received from the PTY master.
    PtyOutput(Vec<u8>),
    /// Keystrokes or mouse events from the user.
    UserInput(Vec<u8>),
    /// Window resize triggers reflow.
    Resize { rows: u16, cols: u16 },
    /// SIGHUP or manual exit.
    Shutdown,
}

/// Abstract PTY interface for dependency injection and mocking.
#[async_trait]
pub trait PtyBackend {
    type Error: std::error::Error;

    /// Spawns the child process and attaches it to the PTY slave.
    async fn spawn(shell: &str, rows: u16, cols: u16) -> Result<Self, Self::Error> where Self: Sized;
    
    /// Reads raw bytes from the PTY master. Should be non-blocking.
    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Self::Error>;
    
    /// Writes raw input bytes to the PTY master.
    async fn write(&mut self, data: &[u8]) -> Result<(), Self::Error>;
    
    /// Sends an IOCTL TIOCSWINSZ to notify the child of layout changes.
    async fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Self::Error>;
}

```

**Implementation Directives:**

* **Backpressure:** Use bounded `tokio::sync::mpsc::channel` for event routing to prevent memory exhaustion if the renderer hangs. Drop or coalesce `Resize` events if the channel is full.
* **Concurrency:** Spawn a dedicated green thread (`tokio::spawn`) solely for blocking `read` calls on the PTY, emitting `TerminalEvent::PtyOutput` to the central orchestrator.
* **Buffer Allocation:** Pre-allocate a fixed-size `[u8; 8192]` buffer for PTY reads to minimize heap allocations during the hot loop.

---

## Phase 2: ANSI Escape Sequence Parsing

**Objective:** Translate the raw PTY byte stream into atomic terminal state mutations.

**Data Structures & Interfaces:**

```rust
use vte::{Params, Perform};

/// The Mutator bridges the stateless VTE parser and the stateful Grid.
pub struct StateMutator<'a> {
    pub grid: &'a mut Grid,
    pub cursor: &'a mut CursorState,
}

impl<'a> Perform for StateMutator<'a> {
    /// Standard printable character insertion.
    fn print(&mut self, c: char) {
        self.grid.write_char(self.cursor, c);
    }

    /// C0 control characters (e.g., \n, \r, \b).
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.grid.line_feed(self.cursor),
            b'\r' => self.cursor.col = 0,
            b'\x08' => self.cursor.backspace(),
            _ => {} // Unhandled C0
        }
    }

    /// ANSI escape sequences (e.g., color changes, cursor movement).
    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            'm' => self.grid.update_attributes(params), // SGR (Select Graphic Rendition)
            'H' => self.cursor.move_to(params),         // Cursor Position
            'J' => self.grid.erase_in_display(params),  // Clear screen variants
            _ => {} // Unimplemented CSI
        }
    }
    
    // Stubbed required trait methods: osc_dispatch, esc_dispatch, hook, put, unhook
}

```

**Implementation Directives:**

* **UTF-8 Boundaries:** The PTY might fragment a 4-byte UTF-8 character across two read buffers. Utilize `vte::Parser::advance`, which inherently handles byte-by-byte state machine transitions without requiring manual UTF-8 buffering logic.
* **Zero-Copy Routing:** Pass parameters by reference. The parser should never allocate memory; it strictly mutates existing state.

---

## Phase 3: Grid State & Scrollback Management

**Objective:** Maintain a highly optimized, cache-friendly memory representation of the screen.

**Data Structures & Interfaces:**

```rust
use std::collections::VecDeque;

/// Bitmask for text attributes to keep struct size minimal.
pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_ITALIC: u16 = 1 << 1;
pub const ATTR_UNDERLINE: u16 = 1 << 2;

/// Optimized to 16 bytes for strict cache alignment.
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
pub struct Cell {
    pub c: char,         // 4 bytes
    pub fg: u32,         // 4 bytes (0x00RRGGBB)
    pub bg: u32,         // 4 bytes (0x00RRGGBB)
    pub flags: u16,      // 2 bytes
    pub _padding: u16,   // 2 bytes (alignment)
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: 0xFFFFFF, bg: 0x000000, flags: 0, _padding: 0 }
    }
}

pub struct Grid {
    cols: usize,
    rows: usize,
    /// 1D array representing the active viewport.
    viewport: Vec<Cell>,
    /// Ring buffer enforcing a strict memory ceiling for historical lines.
    scrollback: VecDeque<Vec<Cell>>,
    max_scrollback: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        Self {
            cols,
            rows,
            viewport: vec![Cell::default(); cols * rows],
            scrollback: VecDeque::with_capacity(max_scrollback),
            max_scrollback,
        }
    }

    /// Translates 2D coordinate to 1D index.
    #[inline(always)]
    pub fn index(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }
}

```

**Implementation Directives:**

* **Memory Ceiling:** When `viewport` lines scroll off-screen, push them to `scrollback`. If `scrollback.len() == max_scrollback`, `pop_front()` to enforce an absolute memory limit.
* **Viewport Reflow:** On window resize, calculate the new 1D vector size. Iterate over existing cells and recalculate their index `y * new_cols + x`. Cells pushed past `new_cols` must wrap to the next line without overwriting existing data.

---

## Phase 4: The Rendering Engine

**Objective:** Decouple the display mechanism to allow swapping TUI (crossterm) and GUI (wgpu) targets, utilizing dirty rectangles to minimize draw latency.

**Data Structures & Interfaces:**

```rust
/// Abstract rendering target.
pub trait Renderer {
    type Error: std::error::Error;

    /// Allocate necessary buffers or initialize target context.
    fn init(&mut self, cols: u16, rows: u16) -> Result<(), Self::Error>;

    /// Compares current grid to previous frame and draws diffs.
    fn render_frame(&mut self, grid: &Grid, cursor: &CursorState) -> Result<(), Self::Error>;

    /// Teardown, flush buffers, and restore target state.
    fn shutdown(&mut self) -> Result<(), Self::Error>;
}

/// Utility for tracking rendering deltas.
pub struct FrameDiff {
    pub previous_viewport: Vec<Cell>,
}

impl FrameDiff {
    /// Yields only the cells that have mutated since the last render pass.
    pub fn calculate_diff<'a>(
        &'a mut self,
        current_grid: &'a Grid,
    ) -> impl Iterator<Item = (usize, usize, &'a Cell)> {
        let cols = current_grid.cols;
        
        current_grid.viewport
            .iter()
            .zip(self.previous_viewport.iter_mut())
            .enumerate()
            .filter_map(move |(idx, (curr, prev))| {
                if curr != prev {
                    *prev = *curr; // Update diff buffer inline
                    let x = idx % cols;
                    let y = idx / cols;
                    Some((x, y, curr))
                } else {
                    None
                }
            })
    }
}

```

**Implementation Directives:**

* **Batching Draw Calls:** In hardware-accelerated implementations (e.g., `wgpu`), avoid creating a quad per mutated cell. Aggregate mutated cells with identical styles into textured vertex batches.
* **Cursor Invalidation:** The cursor position forces a redraw of the underlying cell even if the `Cell` data hasn't changed. Invalidate the old and new cursor coordinates manually within the diffing logic to ensure the cursor block correctly masks and unmasks text.