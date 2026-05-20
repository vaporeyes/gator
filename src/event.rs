// ABOUTME: Phase 1 event vocabulary shared by the PTY, input, and render tasks.
// ABOUTME: Every state change entering the orchestrator is one of these variants.

/// Represents discrete state changes injected into the event loop.
#[derive(Debug)]
pub enum TerminalEvent {
    /// Bytes received from the PTY master.
    PtyOutput(Vec<u8>),
    /// Keystrokes or mouse events from the user.
    UserInput(Vec<u8>),
    /// A bracketed-paste payload; wrapping depends on the active DEC mode.
    /// (Extends the spec enum: bracketed-paste needs the mode state owned
    /// by the orchestrator, so the raw payload is routed through here.)
    Paste(String),
    /// Window resize triggers reflow.
    Resize { rows: u16, cols: u16 },
    /// SIGHUP or manual exit.
    Shutdown,
}
