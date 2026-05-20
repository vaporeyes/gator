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
    /// Mouse event already mapped to grid coords; orchestrator decides whether
    /// to drive local selection or encode passthrough to the PTY.
    Mouse {
        action: MouseAction,
        button: MouseButton,
        col: usize,
        row: usize,
        mods: Mods,
    },
    /// Local scrollback view navigation (no PTY effect).
    Scroll(ScrollCmd),
    /// Window resize triggers reflow.
    Resize { rows: u16, cols: u16 },
    /// SIGHUP or manual exit.
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
pub enum ScrollCmd {
    /// Positive = scroll up into older content; negative = back toward live.
    Lines(isize),
    ToTop,
    ToBottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Drag,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Scroll wheel up; encoded as SGR button 64 in passthrough.
    WheelUp,
    /// Scroll wheel down; encoded as SGR button 65 in passthrough.
    WheelDown,
}

/// Modifier state relevant to mouse and clipboard binds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_: bool,
}
