1. **Spec 1: Event Routing & Grid Coordinate Mapping:** Dependencies: winit, crossterm.
**Goal:** Capture OS-level mouse events, normalize physical/logical pixels into grid coordinates, and inject them into the central orchestrator.

* **Implementation:**
* Expand `TerminalEvent` to include `MouseEvent { action: MouseAction, col: usize, row: usize, modifiers }`. `MouseAction` should enum `Press`, `Drag`, and `Release`.
* **GPU Backend (`app.rs`):** Intercept `winit` `CursorMoved` and `MouseInput`. Map `(physical_x, physical_y)` to `(col, row)` by dividing by `renderer.cell_w` and `renderer.cell_h`.
* **TUI Backend (`main.rs`):** Translate `crossterm::event::MouseEvent` directly to `TerminalEvent::MouseEvent`.


* **Edge Cases to Handle:**
* **Coordinate Clamping:** Drag events can easily exit the window bounds. Coordinates must be clamped strictly to `0..cols-1` and `0..rows-1` before entering the channel.
* **High-Frequency Polling:** `CursorMoved` fires constantly. Only emit `TerminalEvent::MouseEvent(Drag)` if the calculated `(col, row)` actually differs from the previous drag event to avoid flooding the `mpsc` channel.




2. **Spec 2: The Selection State Model:** Memory-bound absolute indexing.
**Goal:** Track the anchor and floating endpoints of the user's selection within the `Grid`.

* **Implementation:**
* Define a `Selection` struct in `grid.rs` with `start` and `end` coordinates.
* **Absolute Row Indexing:** To maintain a selection when the terminal scrolls (e.g., compiling logs), coordinates must use an *absolute* row index (where `0` is the top of the `scrollback` buffer, and `viewport` starts at `scrollback.len()`), not a viewport-relative `y`.
* Provide a `grid.get_selection_text()` method that iterates from `start` to `end`, joining the `c` fields of the `Cell`s.


* **Scalability/Performance:**
* Calculate a normalized `(min_coord, max_coord)` tuple lazily so rendering and text extraction always iterate top-left to bottom-right, regardless of whether the user dragged up or down.


* **Edge Cases to Handle:**
* **Trailing Whitespace:** `get_selection_text()` must strip trailing empty `Cell` spaces at the end of each wrapped line, UNLESS the line was wrapped by `DECAWM` (meaning it's a continuous logical line).




3. **Spec 3: Rendering the Selection Mask:** Requires FrameDiff modification.
**Goal:** Visually invert or highlight selected cells without permanently altering their underlying `Cell` attributes.

* **Implementation:**
* Pass `&Option




4. **Spec 4: Clipboard Integration & Passthrough Bypasses:** Dependencies: arboard.
**Goal:** Bridge the selected text to the host OS clipboard and handle terminal applications that intercept mouse events.

* **Implementation:**
* Integrate the **`arboard`** crate for cross-platform clipboard access.
* On `TerminalEvent::MouseEvent(Release)`, or via a standard keybind (`Ctrl+Shift+C`), extract the string via `grid.get_selection_text()` and push it to `arboard::Clipboard::set_text`.


* **Edge Cases to Handle (Critical):**
* **SGR Mouse Reporting (`CSI ?1006 h`):** If a child application (like `vim` or `htop`) requests mouse tracking, the terminal must *disable* native text selection. Instead, `TerminalEvent::MouseEvent` must be translated into ANSI escape sequences (e.g., `\x1b[<0;col;rowM`) and written directly to the PTY control channel.
* **The Shift Override:** Implement the standard terminal escape hatch: if the `Shift` modifier is held during a mouse click, bypass the child application's SGR mouse mode and force a native terminal text selection.