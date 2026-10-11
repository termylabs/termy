//! Termy's independent terminal engine.
//!
//! The parser, screen storage and protocol state are owned here. Parsing and
//! borrowed screen reads need neither a renderer nor a PTY, and do not depend on
//! Alacritty or the legacy experimental engine. See `README.md` for migration
//! status and the remaining integration gates.

mod dispatch;
mod graphics;
mod grid;
pub mod media;
mod parser;
mod queries;
mod sync;
#[cfg(feature = "pty")]
pub(crate) mod transport;
mod types;
#[cfg(test)]
mod unicode_tests;

use std::collections::VecDeque;
use web_time::Instant;

pub use types::{
    Cell, CellExtra, Color, Cursor, CursorShape, Damage, DirtySpan, Hyperlink, Size, Style,
    UnderlineStyle, ViewportScroll,
};

use dispatch::State;
use parser::Parser;
use sync::SynchronizedUpdate;

const MAX_EVENTS: usize = 1024;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const PARSE_CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub scrollback_history: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scrollback_history: 1000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Bell,
    Title(String),
    ResetTitle,
    WorkingDirectory(String),
    ShellIntegration(String),
    Progress(crate::ProgressState),
    Clipboard { selection: String, data: String },
    KittyClipboard(crate::KittyClipboardOsc),
    KittyClipboardControl(crate::KittyClipboardControl),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTracking {
    #[default]
    None,
    Press,
    Click,
    Drag,
    Motion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    #[default]
    Default,
    Utf8,
    Sgr,
    Urxvt,
    SgrPixels,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub mouse_tracking: MouseTracking,
    pub mouse_encoding: MouseEncoding,
    pub kitty_keyboard: u8,
    pub synchronized_update: bool,
    pub clipboard_paste_events: bool,
}

/// Reusable, single-owner parser and grid. The runtime controls synchronization;
/// the hot parsing path performs no locking and ordinary screen reads borrow.
pub struct Engine {
    parser: Parser,
    synchronized_update: SynchronizedUpdate,
    state: State,
    generation: u64,
}

impl Engine {
    pub fn new(size: Size, options: Options) -> Self {
        Self {
            parser: Parser::with_apc_limit(parser::MAX_APC_BYTES),
            synchronized_update: SynchronizedUpdate::default(),
            state: State::new(size, options),
            generation: 0,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.feed_at(bytes, Instant::now());
    }

    fn feed_at(&mut self, bytes: &[u8], now: Instant) {
        self.state.grid.prepare_output(bytes.len());
        if self
            .synchronized_update_deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            self.stop_synchronized_update();
        }
        let mut offset = 0;
        while offset < bytes.len() {
            if self.synchronized_update_deadline().is_some() {
                let buffered = self.synchronized_update.push(&bytes[offset..], now);
                offset += buffered.consumed;
                if buffered.commit {
                    self.stop_synchronized_update();
                }
            } else {
                let end = offset.saturating_add(PARSE_CHUNK).min(bytes.len());
                let consumed = self.parser.advance(&mut self.state, &bytes[offset..end]);
                offset += consumed;
                self.state.flush_graphics_effects();
                self.generation = self.generation.wrapping_add(1);
                if self.state.modes.synchronized_update {
                    self.synchronized_update
                        .begin(now, self.state.saved_private_mode(2026));
                }
            }
        }
    }

    /// The runtime can schedule a watchdog without polling or copying a frame.
    /// Compare its captured deadline with this value before firing a stale task.
    pub fn synchronized_update_deadline(&self) -> Option<Instant> {
        self.synchronized_update.deadline()
    }

    /// Commit pending output atomically through the existing parser and grid.
    /// Returns false when there is no pending batch (including stale watchdogs).
    pub fn stop_synchronized_update(&mut self) -> bool {
        self.state.grid.release_history_read_cache();
        let Some(bytes) = self.synchronized_update.take_buffer() else {
            return false;
        };
        for chunk in bytes.chunks(PARSE_CHUNK) {
            self.parser.advance_uninterrupted(&mut self.state, chunk);
            self.state.flush_graphics_effects();
        }
        self.state.modes.synchronized_update = false;
        self.state.flush_graphics_effects();
        self.synchronized_update.recycle_buffer(bytes);
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Commit the currently staged synchronized frame while preserving the
    /// application's synchronized-output mode for its next frame.
    pub fn flush_synchronized_update(&mut self) -> bool {
        let was_active = self.state.modes.synchronized_update;
        let saved_mode = self.state.saved_private_mode(2026);
        if !self.stop_synchronized_update() {
            return false;
        }
        if was_active {
            self.synchronized_update.begin(Instant::now(), saved_mode);
            self.state.modes.synchronized_update = true;
        }
        true
    }

    /// Current OSC 7501 records, oldest update first, with inherited apps resolved.
    pub fn program_status(&self) -> Vec<crate::ProgramStatusRecord> {
        self.state.program_status.snapshot()
    }

    pub(crate) fn has_program_status_changes(&self) -> bool {
        self.state.program_status.has_changes()
    }

    /// Coalesced status snapshot, independent of the bounded transient event queue.
    /// An empty snapshot means that the final record was cleared.
    pub fn take_program_status(&mut self) -> Option<Vec<crate::ProgramStatusRecord>> {
        self.state.program_status.take_changed()
    }

    /// Hosts must call this when the process attached to this terminal exits.
    pub fn process_exited(&mut self) {
        self.stop_synchronized_update();
        self.state.program_status.finish();
    }

    pub fn size(&self) -> Size {
        self.state.grid.size()
    }
    pub fn cursor(&self) -> Cursor {
        self.state.grid.cursor
    }
    pub fn modes(&self) -> Modes {
        self.state.modes
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Compact cold scrollback after an output burst. This preserves all cells
    /// and damage; borrowed row views remain available on demand. The native
    /// runtime schedules bounded steps after 250 ms without output, or at most
    /// 2 s after history first needs compaction.
    pub fn compact_history(&mut self) {
        self.state.grid.compact_history(usize::MAX);
    }

    pub(crate) fn take_history_activity(&mut self) -> bool {
        std::mem::take(&mut self.state.grid.history_activity)
    }

    pub(crate) fn needs_history_compaction(&self) -> bool {
        self.state.grid.needs_compaction()
    }
    /// `quiet` steps follow an idle period, so later scrolled rows are packed
    /// as they enter history. Steps forced during output only drain the queue.
    pub(crate) fn compact_history_step(&mut self, rows: usize, quiet: bool) {
        if quiet {
            self.state.grid.compact_history(rows);
        } else {
            self.state.grid.compact_pending_history(rows);
        }
    }

    pub fn history_size(&self) -> usize {
        self.state.grid.history_size()
    }
    pub fn display_offset(&self) -> usize {
        self.state.grid.display_offset()
    }
    pub fn alternate_screen(&self) -> bool {
        self.state.grid.alternate_screen()
    }

    pub fn set_options(&mut self, options: Options) {
        self.state
            .grid
            .set_history_limit(options.scrollback_history);
        self.state.flush_graphics_effects();
        self.generation = self.generation.wrapping_add(1);
    }

    /// The lifetime of the slice prevents mutation while the renderer reads it.
    pub fn viewport_row(&self, row: usize) -> Option<&[Cell]> {
        self.state.grid.visible_row_cells(row)
    }

    /// Visit a viewport row without retaining a dense copy of cold history.
    /// Renderers reuse `scratch` across rows and frames.
    pub(crate) fn with_viewport_row<T>(
        &self,
        row: usize,
        scratch: &mut Vec<Cell>,
        read: impl FnOnce(&[Cell]) -> T,
    ) -> Option<T> {
        self.state
            .grid
            .visible_row(row)
            .map(|row| row.with_cells(scratch, read))
    }

    pub fn viewport_row_wrapped(&self, row: usize) -> bool {
        self.state
            .grid
            .visible_row(row)
            .is_some_and(|row| row.wrapped)
    }

    /// Lines before the live screen are negative, with -1 the newest history row.
    /// A cold history row is expanded on demand. Expanded history is released
    /// on the next output, resize, or scroll; the borrowed slice remains valid
    /// until that exclusive mutation.
    pub fn line(&self, line: i32) -> Option<&[Cell]> {
        self.state.grid.row_cells(line)
    }

    /// Visit a line without retaining a dense copy of cold history. Scratch
    /// capacity is reused across calls, including full-buffer search/copy.
    pub(crate) fn with_line<T>(
        &self,
        line: i32,
        scratch: &mut Vec<Cell>,
        read: impl FnOnce(&[Cell]) -> T,
    ) -> Option<T> {
        self.state
            .grid
            .row(line)
            .map(|row| row.with_cells(scratch, read))
    }

    pub fn line_wrapped(&self, line: i32) -> bool {
        self.state.grid.row(line).is_some_and(|row| row.wrapped)
    }

    pub fn set_query_colors(&mut self, colors: crate::TerminalQueryColors) {
        self.state.query_colors = colors;
    }

    pub fn set_default_cursor_shape(&mut self, shape: CursorShape) {
        self.state.default_cursor_shape = shape;
        if !self.state.cursor_shape_overridden {
            let old = self.state.grid.cursor;
            self.state.grid.cursor.shape = shape;
            self.state.grid.cursor_changed(old);
        }
    }

    pub fn set_cell_pixels(&mut self, width: f32, height: f32) {
        let clamp = |value: f32| {
            if value.is_finite() {
                value.round().clamp(1.0, 65535.0) as u16
            } else {
                1
            }
        };
        self.state.cell_pixels = (clamp(width), clamp(height));
        self.state.resize_graphics();
    }

    pub fn discard_events_and_replies(&mut self) {
        self.state.events.clear();
        self.state.replies.clear();
    }

    pub fn take_damage(&mut self) -> Damage {
        self.state.grid.take_damage()
    }

    /// Replay the ordered row rotations, then patch the damaged spans from the
    /// current viewport. Cell-only consumers can continue using `take_damage`.
    pub fn take_render_damage(&mut self) -> (Damage, Vec<ViewportScroll>) {
        self.state.grid.take_render_damage()
    }

    pub fn resize(&mut self, size: Size) {
        self.stop_synchronized_update();
        self.state.grid.resize(size);
        self.state.resize_graphics();
        self.state.flush_graphics_effects();
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn scroll_display(&mut self, delta: i32) -> bool {
        let changed = self.state.grid.scroll_display(delta);
        if changed {
            self.state.flush_graphics_effects();
            self.generation = self.generation.wrapping_add(1);
        }
        changed
    }

    pub fn clear_scrollback(&mut self) {
        self.state.grid.clear_scrollback();
        self.state.flush_graphics_effects();
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn pop_event(&mut self) -> Option<Event> {
        self.state.events.pop_front()
    }

    /// Draining into a caller-owned buffer allows the PTY writer to reuse it.
    pub fn drain_replies(&mut self, output: &mut Vec<u8>) {
        output.append(&mut self.state.replies);
    }

    pub fn palette(&self) -> &[Option<Color>; 256] {
        &self.state.palette
    }
    pub fn foreground(&self) -> Option<Color> {
        self.state.foreground
    }
    pub fn background(&self) -> Option<Color> {
        self.state.background
    }
    pub fn cursor_color(&self) -> Option<Color> {
        self.state.cursor_color
    }
    pub fn palette_revision(&self) -> u64 {
        self.state.palette_revision
    }

    pub fn graphics_revision(&self) -> u64 {
        self.state.graphics.revision
    }

    /// Advance image animation deadlines without materializing a frame.
    pub fn poll_graphics_revision(&mut self) -> u64 {
        self.state.poll_graphics_revision()
    }

    pub fn graphics_snapshot(&mut self) -> (u64, Vec<crate::KittyGraphicsRenderPlacement>) {
        self.state.graphics_snapshot()
    }

    pub fn graphics_placements(&mut self) -> Vec<crate::KittyGraphicsRenderPlacement> {
        self.graphics_snapshot().1
    }

    /// Reports bounded output queues reaching capacity. The runtime can drain
    /// them between reads; hostile output cannot grow these queues forever.
    pub fn dropped_events(&self) -> u64 {
        self.state.dropped_events
    }
    pub fn dropped_reply_bytes(&self) -> u64 {
        self.state.dropped_reply_bytes
    }
}

fn enqueue(events: &mut VecDeque<Event>, dropped: &mut u64, paste_events: bool, event: Event) {
    if events.len() == MAX_EVENTS {
        if matches!(
            event,
            Event::KittyClipboard(_) | Event::KittyClipboardControl(_)
        ) {
            // Losing a write chunk or a reset must not leave a host with an
            // incomplete clipboard transaction or a stale permission grant.
            // Compaction happens only at capacity, amortizing the cleanup.
            *dropped = dropped.saturating_add(events.len() as u64);
            events.clear();
            events.push_back(Event::KittyClipboardControl(
                crate::KittyClipboardControl::Reset,
            ));
            events.push_back(Event::KittyClipboardControl(
                crate::KittyClipboardControl::Set(paste_events),
            ));
        } else {
            *dropped = dropped.saturating_add(1);
            return;
        }
    }
    events.push_back(event);
}

#[cfg(test)]
mod tests;
