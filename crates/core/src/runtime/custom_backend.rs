//! Adapts the independent parser/grid and native transport to Termy's public
//! terminal API. Renderer callbacks borrow one coherent engine state.

use super::{
    MAX_TERMINAL_SCROLLBACK_HISTORY, TabTitleShellIntegration, TerminalCursorState,
    TerminalCursorStyle, TerminalDamageSnapshot, TerminalDirtySpan, TerminalEvent, TerminalLaunch,
    TerminalOptions, TerminalRuntimeConfig, TerminalSize, TerminalWakeupNotifier,
    resolve_launch_working_directory, resolve_terminal_launch, terminal_environment_overrides,
};
use crate::{
    DetectedLink, DetectedViewportLink, KittyClipboardControl, KittyClipboardHostState,
    KittyClipboardOsc, KittyGraphicsRenderPlacement, TerminalClipboardLocation,
    TerminalClipboardTarget, TerminalColor, TerminalKeyboardMode, TerminalMouseMode,
    TerminalPalette, TerminalQueryColors, TerminalRenderCell, TerminalRenderColor,
    TerminalRenderDamageSnapshot, TerminalRenderRead, TerminalRenderText, TerminalReplyHost,
    TerminalUnderlineStyle, TerminalViewportMetadata, TerminalViewportScroll,
    TerminalViewportScrollDirection, TermyCell, TermyColor, TermyFrame, TermyFrameUpdate,
    TermySearchMatch, TermySearchOptions, TermySharedSearchMatch,
    search::search_lines_shared,
    search_engine::SearchLineMapping,
    terminal_engine::transport::{PtySize, SpawnConfig, Transport},
    terminal_engine::{self as engine, Engine},
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD},
};
use flume::Sender;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

mod maintenance;
mod render;

use render::{
    append_search_cell, cursor_shape, engine_size, expand_legacy_scroll_damage, legacy_cell,
    normalize_spans, pty_size, render_cell, rgb,
};

const EVENT_BATCH: usize = 2048;
const MAX_EVENTS: usize = 65_536;
// Count limits alone allow multi-gigabyte backlogs of maximum-size OSC strings.
// Track retained heap capacity independently of the fixed-size queue slots.
const MAX_EVENT_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_REPLIES: usize = 2 * 1024 * 1024;
const PARSE_BATCH: usize = 4096;
const HISTORY_COMPACTION_IDLE_DELAY: Duration = Duration::from_millis(250);
const HISTORY_COMPACTION_MAX_DELAY: Duration = Duration::from_secs(2);

pub(super) struct CustomBackend {
    shared: Arc<Shared>,
    transport: Option<Arc<Transport>>,
}

struct Shared {
    state: Mutex<State>,
    clipboard: Mutex<KittyClipboardHostState>,
    notifier: Option<TerminalWakeupNotifier>,
    wakeup_enabled: AtomicBool,
    wakeup_queued: AtomicBool,
    transport: Mutex<Weak<Transport>>,
    sync_signal: Arc<SyncSignal>,
    sync_watchdog_started: AtomicBool,
}

#[derive(Default)]
struct SyncSignal {
    state: Mutex<SyncSignalState>,
    changed: Condvar,
}

#[derive(Default)]
struct SyncSignalState {
    deadline: Option<Instant>,
    shutdown: bool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.sync_signal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown = true;
        self.sync_signal.changed.notify_one();
    }
}

struct State {
    engine: Engine,
    size: TerminalSize,
    query_colors: TerminalQueryColors,
    default_cursor_style: TerminalCursorStyle,
    events: VecDeque<PendingEvent>,
    event_payload_bytes: usize,
    replies: Vec<u8>,
    generation: u64,
    palette_epoch: u64,
    force_full_damage: bool,
    last_damage_cursor: Option<TerminalCursorState>,
    history_deadline: Option<Instant>,
    // First output that left history uncompacted. Bounds how long steady
    // output can keep postponing the idle compaction deadline.
    history_pending_since: Option<Instant>,
    // Whether the pending deadline is the idle delay rather than the cap.
    history_quiet: bool,
    // A title event was queued since the last feed or commit woke the host.
    title_changed: bool,
}

enum PendingEvent {
    Terminal(TerminalEvent),
    ClipboardLoad(String),
    Kitty(KittyClipboardOsc),
    KittyControl(KittyClipboardControl),
    KittyOverflow,
}

impl PendingEvent {
    fn payload_bytes(&self) -> usize {
        match self {
            Self::Terminal(
                TerminalEvent::Title(text)
                | TerminalEvent::WorkingDirectory(text)
                | TerminalEvent::ClipboardStore(text),
            )
            | Self::ClipboardLoad(text) => text.capacity(),
            Self::Kitty(packet) => packet.payload_capacity(),
            Self::Terminal(_) | Self::KittyControl(_) | Self::KittyOverflow => 0,
        }
    }

    fn must_deliver(&self) -> bool {
        matches!(
            self,
            Self::Terminal(TerminalEvent::Exit)
                | Self::KittyOverflow
                | Self::KittyControl(KittyClipboardControl::Set(_) | KittyClipboardControl::Reset)
        )
    }
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self) {
        if self.wakeup_enabled.load(Ordering::Acquire) {
            self.queue_wakeup();
        }
    }

    // Titles and program status drive tab chrome, which stays visible for
    // hidden tabs, so they wake the host even while render wakeups are suspended.
    fn notify_tab_chrome(&self) {
        self.queue_wakeup();
    }

    fn queue_wakeup(&self) {
        if !self.wakeup_queued.swap(true, Ordering::AcqRel) {
            crate::render_metrics::increment_runtime_wakeup_count();
            if let Some(notifier) = &self.notifier {
                notifier.notify();
            }
        }
    }

    /// Publish any staged synchronized frame before forwarding new user input.
    /// Interactive TUIs can keep a frame open while they redraw; committing it
    /// at the input boundary prevents keystrokes from appearing to have no
    /// effect until a later redraw.
    fn commit_synchronized_output_before_input(self: &Arc<Self>) {
        let mut state = self.state();
        if !state.engine.flush_synchronized_update() {
            return;
        }
        if state.engine.take_history_activity() {
            state.defer_history_compaction(Instant::now());
        }
        while let Some(event) = state.engine.pop_event() {
            state.engine_event(event);
        }
        let mut replies = Vec::new();
        state.engine.drain_replies(&mut replies);
        state.generation = state.generation.wrapping_add(1);
        self.schedule_maintenance(&state);
        drop(state);

        if !replies.is_empty() {
            let transport = self
                .transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .upgrade();
            if let Some(transport) = transport {
                if let Err(error) = transport.write_protocol_reply_owned(replies) {
                    log::warn!("terminal input-boundary reply failed: {error}");
                }
            } else {
                self.state().append_replies(&replies);
            }
        }
        self.notify();
    }

    fn feed(self: &Arc<Self>, bytes: &[u8], hydrate: bool, buffer_replies: bool) -> Vec<u8> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let mut state = self.state();
        let mut replies = state.feed(bytes, hydrate);
        if buffer_replies {
            state.append_replies(&replies);
            replies.clear();
        }
        if state.engine.take_history_activity() {
            state.defer_history_compaction(Instant::now());
        }
        let should_notify = !state.engine.modes().synchronized_update || !state.events.is_empty();
        let tab_chrome_changed = state.take_tab_chrome_changed();
        // Publish while the engine lock still protects this deadline: a later
        // feed must not have its timer replaced by an earlier feed's deadline.
        self.schedule_maintenance(&state);
        drop(state);
        if should_notify {
            self.notify();
        }
        if tab_chrome_changed {
            self.notify_tab_chrome();
        }
        replies
    }
}

impl State {
    /// Compact after output has been quiet for a short delay, but never wait
    /// longer than the maximum delay, so a steady trickle still gets packed.
    fn defer_history_compaction(&mut self, now: Instant) {
        if !self.engine.needs_history_compaction() {
            self.history_deadline = None;
            self.history_pending_since = None;
            return;
        }
        let since = *self.history_pending_since.get_or_insert(now);
        let idle = now + HISTORY_COMPACTION_IDLE_DELAY;
        let cap = since + HISTORY_COMPACTION_MAX_DELAY;
        self.history_quiet = idle <= cap;
        self.history_deadline = Some(idle.min(cap));
    }

    fn maintenance_deadline(&self) -> Option<Instant> {
        self.engine
            .synchronized_update_deadline()
            .into_iter()
            .chain(self.history_deadline)
            .min()
    }

    fn new(size: TerminalSize, config: &TerminalRuntimeConfig) -> Self {
        let mut engine = Engine::new(
            engine_size(size.clamped()),
            engine::Options {
                scrollback_history: config
                    .scrollback_history
                    .min(MAX_TERMINAL_SCROLLBACK_HISTORY),
            },
        );
        engine.set_default_cursor_shape(cursor_shape(config.default_cursor_style));
        engine.set_query_colors(config.query_colors);
        engine.set_cell_pixels(size.cell_width, size.cell_height);
        let actual = engine.size();
        Self {
            engine,
            size: TerminalSize {
                cols: actual.cols as u16,
                rows: actual.rows as u16,
                ..size.clamped()
            },
            query_colors: config.query_colors,
            default_cursor_style: config.default_cursor_style,
            events: VecDeque::new(),
            event_payload_bytes: 0,
            replies: Vec::new(),
            generation: 0,
            palette_epoch: 0,
            force_full_damage: true,
            last_damage_cursor: None,
            history_deadline: None,
            history_pending_since: None,
            history_quiet: true,
            title_changed: false,
        }
    }

    fn queue(&mut self, event: PendingEvent) {
        let bytes = event.payload_bytes();
        if bytes > MAX_EVENT_PAYLOAD_BYTES.saturating_sub(self.event_payload_bytes) {
            self.discard_event(event);
            return;
        }
        if self.events.len() == MAX_EVENTS {
            if !event.must_deliver() {
                self.discard_event(event);
                return;
            }
            self.compact_saturated_events();
        }
        self.event_payload_bytes += bytes;
        self.events.push_back(event);
    }

    fn discard_event(&mut self, event: PendingEvent) {
        if matches!(event, PendingEvent::Kitty(_)) {
            // A missing chunk must not allow a later terminator to commit a
            // truncated clipboard write. Keep this abort ordered with packets.
            self.queue(PendingEvent::KittyOverflow);
        }
    }

    fn pop_event(&mut self) -> Option<PendingEvent> {
        let event = self.events.pop_front()?;
        self.event_payload_bytes -= event.payload_bytes();
        Some(event)
    }

    // A critical event must survive even a queue filled entirely with controls.
    // Discard the saturated ordinary backlog and retain cumulative clipboard
    // invalidation, final paste mode, and process exit. All pending clipboard
    // packets are discarded together, so compression cannot move them across a
    // permission reset. This O(n) pass occurs only after MAX_EVENTS enqueues.
    fn compact_saturated_events(&mut self) {
        let mut reset = false;
        let mut revoke_grants = false;
        let mut clipboard_overflow = false;
        let mut paste_mode = None;
        let mut exit = false;
        while let Some(event) = self.pop_event() {
            match event {
                PendingEvent::KittyControl(KittyClipboardControl::Reset) => {
                    reset = true;
                    paste_mode = Some(false);
                }
                PendingEvent::KittyControl(KittyClipboardControl::Set(enabled)) => {
                    revoke_grants |= !enabled;
                    paste_mode = Some(enabled);
                }
                PendingEvent::Terminal(TerminalEvent::Exit) => exit = true,
                PendingEvent::Kitty(_) | PendingEvent::KittyOverflow => clipboard_overflow = true,
                _ => {}
            }
        }
        if reset {
            self.queue(PendingEvent::KittyControl(KittyClipboardControl::Reset));
        } else if clipboard_overflow {
            self.queue(PendingEvent::KittyOverflow);
        }
        if revoke_grants && !reset {
            self.queue(PendingEvent::KittyControl(KittyClipboardControl::Set(
                false,
            )));
        }
        if paste_mode == Some(true) {
            self.queue(PendingEvent::KittyControl(KittyClipboardControl::Set(true)));
        }
        if exit {
            self.queue(PendingEvent::Terminal(TerminalEvent::Exit));
        }
    }

    fn feed(&mut self, bytes: &[u8], hydrate: bool) -> Vec<u8> {
        let before_generation = self.engine.generation();
        let mut replies = Vec::new();
        for bytes in bytes.chunks(PARSE_BATCH) {
            self.engine.feed(bytes);
            while let Some(event) = self.engine.pop_event() {
                if !hydrate {
                    self.engine_event(event);
                }
            }
            self.engine.drain_replies(&mut replies);
            if hydrate {
                replies.clear();
            }
        }
        if hydrate {
            self.engine.stop_synchronized_update();
            while self.engine.pop_event().is_some() {}
            self.engine.drain_replies(&mut replies);
            replies.clear();
        }
        if self.engine.generation() != before_generation {
            self.generation = self.generation.wrapping_add(1);
        }
        replies
    }

    fn take_tab_chrome_changed(&mut self) -> bool {
        std::mem::take(&mut self.title_changed) || self.engine.has_program_status_changes()
    }

    fn engine_event(&mut self, event: engine::Event) {
        let event = match event {
            engine::Event::KittyClipboard(packet) => {
                self.queue(PendingEvent::Kitty(packet));
                return;
            }
            engine::Event::KittyClipboardControl(control) => {
                self.queue(PendingEvent::KittyControl(control));
                return;
            }
            engine::Event::Bell => TerminalEvent::Bell,
            engine::Event::Title(title) => {
                self.title_changed = true;
                TerminalEvent::Title(title)
            }
            engine::Event::ResetTitle => {
                self.title_changed = true;
                TerminalEvent::ResetTitle
            }
            engine::Event::Progress(progress) => TerminalEvent::Progress(progress),
            engine::Event::WorkingDirectory(path) => TerminalEvent::WorkingDirectory(path),
            engine::Event::ShellIntegration(value) => {
                let mut parts = value.split(';');
                match parts.next() {
                    Some("A") => TerminalEvent::ShellPromptStart,
                    Some("B") => TerminalEvent::ShellCommandStart,
                    Some("C") => TerminalEvent::ShellCommandExecuting,
                    Some("D") => TerminalEvent::ShellCommandFinished(
                        parts.next().and_then(|code| code.parse().ok()),
                    ),
                    _ => return,
                }
            }
            engine::Event::Clipboard { selection, data } => {
                if data == "?" {
                    self.queue(PendingEvent::ClipboardLoad(selection));
                    return;
                }
                let Ok(bytes) = STANDARD_NO_PAD
                    .decode(data.trim_end_matches('=').as_bytes())
                    .or_else(|_| BASE64.decode(&data))
                else {
                    return;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    return;
                };
                TerminalEvent::ClipboardStore(text)
            }
        };
        self.queue(PendingEvent::Terminal(event));
    }

    fn append_replies(&mut self, bytes: &[u8]) {
        let available = MAX_PENDING_REPLIES.saturating_sub(self.replies.len());
        if bytes.len() <= available {
            self.replies.extend_from_slice(bytes);
        } else {
            log::warn!("terminal protocol reply queue is full");
        }
    }

    fn cursor(&self) -> Option<TerminalCursorState> {
        let cursor = self.engine.cursor();
        let row = cursor.row.saturating_add(self.engine.display_offset());
        (cursor.visible && row < self.engine.size().rows).then_some(TerminalCursorState {
            row,
            col: cursor.col,
            style: match cursor.shape {
                engine::CursorShape::Block => TerminalCursorStyle::Block,
                engine::CursorShape::Beam | engine::CursorShape::Underline => {
                    TerminalCursorStyle::Line
                }
            },
        })
    }

    fn metadata(&self) -> TerminalViewportMetadata {
        TerminalViewportMetadata {
            cols: self.size.cols,
            rows: self.size.rows,
            cursor: self.cursor(),
            display_offset: self.engine.display_offset(),
            history_size: self.engine.history_size(),
            palette_revision: self
                .engine
                .palette_revision()
                .wrapping_add(self.palette_epoch),
            generation: self.generation,
        }
    }

    fn palette(&self) -> TerminalPalette {
        TerminalPalette {
            indexed: std::array::from_fn(|index| self.engine.palette()[index].and_then(rgb)),
            foreground: self.engine.foreground().and_then(rgb),
            background: self.engine.background().and_then(rgb),
            cursor: self.engine.cursor_color().and_then(rgb),
            revision: self
                .engine
                .palette_revision()
                .wrapping_add(self.palette_epoch),
        }
    }

    fn take_damage(&mut self, force_full: bool) -> TerminalRenderDamageSnapshot {
        let (source, source_scrolls) = self.engine.take_render_damage();
        let pending_full = std::mem::take(&mut self.force_full_damage);
        let full = force_full || pending_full;
        let mut damage = if full {
            TerminalDamageSnapshot::Full
        } else {
            match source {
                engine::Damage::Full => TerminalDamageSnapshot::Full,
                engine::Damage::Partial(spans) => TerminalDamageSnapshot::Partial(
                    spans
                        .into_iter()
                        .filter(|span| span.start < span.end)
                        .map(|span| TerminalDirtySpan {
                            row: span.row,
                            left_col: span.start.saturating_sub(1),
                            right_col: span.end.min(self.engine.size().cols.saturating_sub(1)),
                        })
                        .collect(),
                ),
            }
        };
        let scrolls: Vec<_> = if matches!(damage, TerminalDamageSnapshot::Full) {
            Vec::new()
        } else {
            source_scrolls
                .into_iter()
                .map(|scroll| TerminalViewportScroll {
                    top: scroll.top,
                    bottom: scroll.bottom - 1,
                    count: scroll.lines.unsigned_abs() as usize,
                    direction: if scroll.lines > 0 {
                        TerminalViewportScrollDirection::Up
                    } else {
                        TerminalViewportScrollDirection::Down
                    },
                })
                .collect()
        };
        let previous_cursor = self.last_damage_cursor.and_then(|mut cursor| {
            for scroll in &scrolls {
                if !(scroll.top..=scroll.bottom).contains(&cursor.row) {
                    continue;
                }
                cursor.row = match scroll.direction {
                    TerminalViewportScrollDirection::Up => cursor.row.checked_sub(scroll.count)?,
                    TerminalViewportScrollDirection::Down => {
                        cursor.row.checked_add(scroll.count)?
                    }
                };
                if !(scroll.top..=scroll.bottom).contains(&cursor.row) {
                    return None;
                }
            }
            Some(cursor)
        });
        let cursor = self.cursor();
        if cursor != self.last_damage_cursor || !scrolls.is_empty() {
            if let TerminalDamageSnapshot::Partial(spans) = &mut damage {
                for cursor in previous_cursor.into_iter().chain(cursor) {
                    if cursor.row < self.engine.size().rows {
                        spans.push(TerminalDirtySpan {
                            row: cursor.row,
                            left_col: cursor.col.saturating_sub(1),
                            right_col: cursor
                                .col
                                .saturating_add(1)
                                .min(self.engine.size().cols - 1),
                        });
                    }
                }
                normalize_spans(spans);
            }
            self.last_damage_cursor = cursor;
        }
        TerminalRenderDamageSnapshot {
            damage,
            scrolls,
            generation: self.generation,
            palette_revision: self.metadata().palette_revision,
        }
    }

    fn render_read(&mut self, force_full: bool) -> TerminalRenderRead {
        let update = self.take_damage(force_full);
        let mut cells =
            Vec::with_capacity(usize::from(self.size.cols) * usize::from(self.size.rows));
        let mut scratch = Vec::new();
        for row in 0..usize::from(self.size.rows) {
            let wrapped = self.engine.viewport_row_wrapped(row);
            self.engine.with_viewport_row(row, &mut scratch, |source| {
                for (col, cell) in source.iter().enumerate() {
                    cells.push(render_cell(cell, wrapped && col + 1 == source.len()));
                }
            });
        }
        TerminalRenderRead {
            metadata: self.metadata(),
            palette: self.palette(),
            cells,
            update,
        }
    }
}

impl CustomBackend {
    pub(super) fn new(
        size: TerminalSize,
        working_dir: Option<&str>,
        wakeup: Option<Sender<()>>,
        shell_integration: Option<&TabTitleShellIntegration>,
        config: Option<&TerminalRuntimeConfig>,
        startup: Option<&str>,
    ) -> anyhow::Result<Self> {
        let notifier = wakeup.map(|sender| {
            TerminalWakeupNotifier::new(move || {
                let _ = sender.try_send(());
            })
        });
        Self::new_with_wakeup_notifier(
            size,
            working_dir,
            notifier,
            shell_integration,
            config,
            startup,
        )
    }

    pub(super) fn new_with_wakeup_notifier(
        size: TerminalSize,
        working_dir: Option<&str>,
        notifier: Option<TerminalWakeupNotifier>,
        shell_integration: Option<&TabTitleShellIntegration>,
        config: Option<&TerminalRuntimeConfig>,
        startup: Option<&str>,
    ) -> anyhow::Result<Self> {
        let launch = startup.map(|command| TerminalLaunch::ShellCommand(command.to_owned()));
        Self::new_with_launch_and_wakeup_notifier(
            size,
            working_dir,
            notifier,
            shell_integration,
            config,
            launch.as_ref(),
        )
    }

    pub(super) fn new_with_launch_and_wakeup_notifier(
        size: TerminalSize,
        working_dir: Option<&str>,
        notifier: Option<TerminalWakeupNotifier>,
        shell_integration: Option<&TabTitleShellIntegration>,
        config: Option<&TerminalRuntimeConfig>,
        launch: Option<&TerminalLaunch>,
    ) -> anyhow::Result<Self> {
        if !engine::transport::available() {
            anyhow::bail!("the native terminal transport is unavailable on this host");
        }
        let config = config.cloned().unwrap_or_default();
        let mut terminal = Self::new_display_with_wakeup_notifier(size, Some(&config), notifier);
        let launch = resolve_terminal_launch(&config, launch)?;
        let spawn = SpawnConfig {
            program: launch.program,
            args: launch.args,
            working_directory: resolve_launch_working_directory(
                working_dir,
                config.working_dir_fallback,
            ),
            environment: terminal_environment_overrides(shell_integration, &config)
                .into_iter()
                .collect(),
        };
        let shared = terminal.shared.clone();
        let exit = terminal.shared.clone();
        terminal.transport = Some(Arc::new(Transport::spawn(
            spawn,
            pty_size(terminal.size()),
            move |bytes| shared.feed(bytes, false, false),
            move || {
                let mut state = exit.state();
                if state.engine.stop_synchronized_update() {
                    while let Some(event) = state.engine.pop_event() {
                        state.engine_event(event);
                    }
                    let mut replies = Vec::new();
                    state.engine.drain_replies(&mut replies);
                    state.append_replies(&replies);
                    state.generation = state.generation.wrapping_add(1);
                }
                state.engine.process_exited();
                state.queue(PendingEvent::Terminal(TerminalEvent::Exit));
                drop(state);
                exit.notify();
            },
        )?));
        *terminal
            .shared
            .transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Arc::downgrade(terminal.transport.as_ref().unwrap());
        Ok(terminal)
    }

    pub(super) fn new_display(size: TerminalSize, config: Option<&TerminalRuntimeConfig>) -> Self {
        Self::new_display_with_wakeup_notifier(size, config, None)
    }

    pub(super) fn new_display_with_wakeup_notifier(
        size: TerminalSize,
        config: Option<&TerminalRuntimeConfig>,
        notifier: Option<TerminalWakeupNotifier>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::new(size, &config.cloned().unwrap_or_default())),
                clipboard: Mutex::new(KittyClipboardHostState::new()),
                notifier,
                wakeup_enabled: AtomicBool::new(true),
                wakeup_queued: AtomicBool::new(false),
                transport: Mutex::new(Weak::new()),
                sync_signal: Arc::new(SyncSignal::default()),
                sync_watchdog_started: AtomicBool::new(false),
            }),
            transport: None,
        }
    }

    pub(super) fn feed_output(&self, bytes: &[u8]) {
        let replies = self.shared.feed(bytes, false, self.transport.is_none());
        self.send_reply(replies);
    }
    pub(super) fn hydrate_output(&self, bytes: &[u8]) {
        self.shared.feed(bytes, true, false);
    }
    pub(super) fn child_pid(&self) -> Option<u32> {
        self.transport
            .as_ref()
            .map(|transport| transport.child_pid())
    }
    pub(super) fn set_wakeup_enabled(&self, enabled: bool) {
        let old = self.shared.wakeup_enabled.swap(enabled, Ordering::AcqRel);
        if enabled && !old {
            self.shared.wakeup_queued.store(false, Ordering::Release);
            self.shared.notify();
        }
    }
    pub(super) fn write(&self, input: &[u8]) {
        if input.is_empty() {
            return;
        }
        self.shared.commit_synchronized_output_before_input();
        if let Some(transport) = &self.transport
            && let Err(error) = transport.write(input)
        {
            log::warn!("terminal input failed: {error}");
        }
    }
    pub(super) fn write_owned(&self, input: Vec<u8>) {
        if input.is_empty() {
            return;
        }
        self.shared.commit_synchronized_output_before_input();
        if let Some(transport) = &self.transport
            && let Err(error) = transport.write_owned(input)
        {
            log::warn!("terminal input failed: {error}");
        }
    }
    pub(super) fn write_str(&self, input: &str) {
        self.write(input.as_bytes());
    }
    fn send_reply(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Some(transport) = &self.transport {
            if let Err(error) = transport.write_protocol_reply_owned(bytes) {
                log::warn!("terminal reply failed: {error}");
            }
        } else {
            self.shared.state().append_replies(&bytes);
        }
    }

    pub(super) fn resize(&mut self, size: TerminalSize) {
        let mut state = self.shared.state();
        let size = size.clamped();
        if state.size == size {
            return;
        }
        state.engine.resize(engine_size(size));
        state
            .engine
            .set_cell_pixels(size.cell_width, size.cell_height);
        if state.engine.take_history_activity() {
            state.defer_history_compaction(Instant::now());
        }
        let actual = state.engine.size();
        let size = TerminalSize {
            cols: actual.cols as u16,
            rows: actual.rows as u16,
            ..size
        };
        state.size = size;
        state.generation = state.generation.wrapping_add(1);
        state.force_full_damage = true;
        while let Some(event) = state.engine.pop_event() {
            state.engine_event(event);
        }
        let mut replies = Vec::new();
        state.engine.drain_replies(&mut replies);
        self.shared.schedule_maintenance(&state);
        drop(state);
        self.send_reply(replies);
        if let Some(transport) = &self.transport
            && let Err(error) = transport.resize(pty_size(size))
        {
            log::warn!("terminal resize failed: {error}");
        }
        self.shared.notify();
    }
    pub(super) fn nudge_resize(&self) {
        if let Some(transport) = &self.transport
            && let Err(error) = transport.resize(pty_size(self.size()))
        {
            log::warn!("terminal resize notification failed: {error}");
        }
    }
    pub(super) fn size(&self) -> TerminalSize {
        self.shared.state().size
    }

    pub(super) fn kitty_graphics_snapshot(&self) -> (u64, Vec<KittyGraphicsRenderPlacement>) {
        let mut state = self.shared.state();
        state.engine.graphics_snapshot()
    }
    pub(super) fn kitty_graphics_placements(&self) -> Vec<KittyGraphicsRenderPlacement> {
        self.kitty_graphics_snapshot().1
    }
    pub(super) fn kitty_graphics_revision(&self) -> u64 {
        self.shared.state().engine.poll_graphics_revision()
    }
    pub(super) fn kitty_clipboard_paste_events_enabled(&self) -> bool {
        self.shared.state().engine.modes().clipboard_paste_events
    }
    pub(super) fn kitty_clipboard_paste_notification(
        &self,
        location: TerminalClipboardLocation,
        formats: &[String],
    ) -> Option<Vec<u8>> {
        let enabled = self.kitty_clipboard_paste_events_enabled();
        let mut clipboard = self
            .shared
            .clipboard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clipboard.set_paste_events_enabled(enabled);
        clipboard.paste_notification(location, formats)
    }
    pub(super) fn send_kitty_clipboard_paste_event(
        &self,
        location: TerminalClipboardLocation,
        formats: &[String],
    ) -> bool {
        let Some(reply) = self.kitty_clipboard_paste_notification(location, formats) else {
            return false;
        };
        self.send_reply(reply);
        true
    }

    pub(super) fn drain_events(
        &self,
        host: &mut impl TerminalReplyHost,
    ) -> (Vec<TerminalEvent>, bool) {
        let wakeup = self.shared.wakeup_queued.swap(false, Ordering::AcqRel);
        let (batch, has_more, program_status) = {
            let mut state = self.shared.state();
            let count = state.events.len().min(EVENT_BATCH);
            let batch: Vec<_> = (0..count).filter_map(|_| state.pop_event()).collect();
            let program_status = state.engine.take_program_status();
            (batch, !state.events.is_empty(), program_status)
        };
        let mut events = Vec::with_capacity(batch.len() + usize::from(wakeup));
        if wakeup {
            events.push(TerminalEvent::Wakeup);
        }
        // Deliver current state before Exit; it is not subject to queue overflow.
        if let Some(records) = program_status {
            events.push(TerminalEvent::ProgramStatus(records));
        }
        for event in batch {
            match event {
                PendingEvent::Terminal(event) => events.push(event),
                PendingEvent::ClipboardLoad(selection) => {
                    let target = if selection.contains('c') || selection.is_empty() {
                        TerminalClipboardTarget::Clipboard
                    } else {
                        TerminalClipboardTarget::Selection
                    };
                    if let Some(text) = host.load_clipboard(target) {
                        self.send_reply(
                            format!("\x1b]52;{selection};{}\x1b\\", BASE64.encode(text))
                                .into_bytes(),
                        );
                    }
                }
                PendingEvent::Kitty(packet) => {
                    let replies = self
                        .shared
                        .clipboard
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .handle_osc(packet, host);
                    for reply in replies {
                        self.send_reply(reply);
                    }
                }
                PendingEvent::KittyOverflow => {
                    self.shared
                        .clipboard
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .reset_preserving_paste_mode();
                }
                PendingEvent::KittyControl(control) => {
                    let mut clipboard = self
                        .shared
                        .clipboard
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match control {
                        KittyClipboardControl::Set(enabled) => {
                            clipboard.set_paste_events_enabled(enabled);
                        }
                        KittyClipboardControl::Reset => clipboard.reset(),
                        KittyClipboardControl::Query => {}
                    }
                }
            }
        }
        let replies = {
            let mut state = self.shared.state();
            std::mem::take(&mut state.replies)
        };
        if !replies.is_empty() {
            host.protocol_reply(&replies);
        }
        (events, has_more)
    }
    pub(super) fn has_pending_events(&self) -> bool {
        let state = self.shared.state();
        self.shared.wakeup_queued.load(Ordering::Acquire)
            || !state.events.is_empty()
            || !state.replies.is_empty()
            || state.engine.has_program_status_changes()
    }
    pub(super) fn set_query_colors(&mut self, colors: TerminalQueryColors) {
        let mut state = self.shared.state();
        state.query_colors = colors;
        state.engine.set_query_colors(colors);
        state.palette_epoch = state.palette_epoch.wrapping_add(1);
        state.force_full_damage = true;
    }
    pub(super) fn palette(&self) -> TerminalPalette {
        self.shared.state().palette()
    }
    pub(super) fn snapshot(&self) -> TermyFrame {
        let state = self.shared.state();
        let palette = state.palette();
        let mut cells =
            Vec::with_capacity(usize::from(state.size.cols) * usize::from(state.size.rows));
        let query_colors = state.query_colors;
        let mut scratch = Vec::new();
        for row in 0..usize::from(state.size.rows) {
            let wrapped = state.engine.viewport_row_wrapped(row);
            state.engine.with_viewport_row(row, &mut scratch, |line| {
                for (col, cell) in line.iter().enumerate() {
                    cells.push(legacy_cell(
                        cell,
                        wrapped && col + 1 == line.len(),
                        &palette,
                        query_colors,
                    ));
                }
            });
        }
        TermyFrame {
            cols: state.size.cols,
            rows: state.size.rows,
            cells,
            cursor: state.cursor(),
            display_offset: state.engine.display_offset(),
            history_size: state.engine.history_size(),
        }
    }
    pub(super) fn frame_update(&self, force_full: bool) -> TermyFrameUpdate {
        let mut state = self.shared.state();
        let mut update = state.take_damage(force_full);
        expand_legacy_scroll_damage(&mut update, usize::from(state.size.cols));
        let palette = state.palette();
        let spans = match &update.damage {
            TerminalDamageSnapshot::Full => (0..usize::from(state.size.rows))
                .map(|row| TerminalDirtySpan {
                    row,
                    left_col: 0,
                    right_col: usize::from(state.size.cols) - 1,
                })
                .collect(),
            TerminalDamageSnapshot::Partial(spans) => spans.clone(),
        };
        let mut cells = Vec::new();
        let query_colors = state.query_colors;
        let mut scratch = Vec::new();
        for span in spans {
            let wrapped = state.engine.viewport_row_wrapped(span.row);
            state
                .engine
                .with_viewport_row(span.row, &mut scratch, |line| {
                    let right = span.right_col.min(line.len() - 1);
                    for col in span.left_col..=right {
                        cells.push(legacy_cell(
                            &line[col],
                            wrapped && col + 1 == line.len(),
                            &palette,
                            query_colors,
                        ));
                    }
                });
        }
        TermyFrameUpdate {
            cols: state.size.cols,
            rows: state.size.rows,
            cells,
            cursor: state.cursor(),
            display_offset: state.engine.display_offset(),
            history_size: state.engine.history_size(),
            damage: update.damage,
        }
    }
    pub(super) fn take_render_damage_snapshot(&self) -> TerminalRenderDamageSnapshot {
        self.shared.state().take_damage(false)
    }
    pub(super) fn take_damage_snapshot(&self) -> TerminalDamageSnapshot {
        let mut state = self.shared.state();
        let mut update = state.take_damage(false);
        expand_legacy_scroll_damage(&mut update, usize::from(state.size.cols));
        update.damage
    }
    pub(super) fn render_read(&self, force_full: bool) -> TerminalRenderRead {
        self.shared.state().render_read(force_full)
    }
    pub(super) fn render_read_with_screen(&self, force_full: bool) -> (TerminalRenderRead, bool) {
        let mut state = self.shared.state();
        let alternate = state.engine.alternate_screen();
        (state.render_read(force_full), alternate)
    }
    pub(super) fn visit_viewport_cells(
        &self,
        mut visitor: impl FnMut(usize, i32, usize, &TerminalRenderCell),
    ) -> TerminalViewportMetadata {
        // Preserve reentrant callbacks at the public snapshot boundary. The
        // desktop renderer uses the explicit locked variant below instead.
        let mut cells = Vec::new();
        let metadata = self.visit_viewport_cells_locked(|_, _, _, cell| cells.push(cell.clone()));
        let cols = usize::from(metadata.cols);
        for (index, cell) in cells.iter().enumerate() {
            visitor(
                metadata.display_offset,
                (index / cols) as i32 - metadata.display_offset as i32,
                index % cols,
                cell,
            );
        }
        metadata
    }
    pub(super) fn visit_viewport_cells_locked(
        &self,
        mut visitor: impl FnMut(usize, i32, usize, &TerminalRenderCell),
    ) -> TerminalViewportMetadata {
        let state = self.shared.state();
        let offset = state.engine.display_offset();
        let mut scratch = Vec::new();
        for row in 0..usize::from(state.size.rows) {
            let wrapped = state.engine.viewport_row_wrapped(row);
            state.engine.with_viewport_row(row, &mut scratch, |line| {
                for (col, cell) in line.iter().enumerate() {
                    visitor(
                        offset,
                        row as i32 - offset as i32,
                        col,
                        &render_cell(cell, wrapped && col + 1 == line.len()),
                    );
                }
            });
        }
        state.metadata()
    }
    pub(super) fn visit_viewport_ranges_at_generation(
        &self,
        generation: u64,
        spans: &[TerminalDirtySpan],
        mut visitor: impl FnMut(usize, usize, i32, usize, &TerminalRenderCell),
    ) -> bool {
        let mut cells = Vec::new();
        if !self.visit_viewport_ranges_locked_at_generation(
            generation,
            spans,
            |row, offset, line, col, cell| cells.push((row, offset, line, col, cell.clone())),
        ) {
            return false;
        }
        for (row, offset, line, col, cell) in cells {
            visitor(row, offset, line, col, &cell);
        }
        true
    }
    pub(super) fn visit_viewport_ranges_locked_at_generation(
        &self,
        generation: u64,
        spans: &[TerminalDirtySpan],
        mut visitor: impl FnMut(usize, usize, i32, usize, &TerminalRenderCell),
    ) -> bool {
        let state = self.shared.state();
        if state.generation != generation {
            return false;
        }
        let offset = state.engine.display_offset();
        let mut scratch = Vec::new();
        for span in spans {
            let wrapped = state.engine.viewport_row_wrapped(span.row);
            state
                .engine
                .with_viewport_row(span.row, &mut scratch, |line| {
                    let end = span.right_col.saturating_add(1).min(line.len());
                    for col in span.left_col.min(end)..end {
                        visitor(
                            span.row,
                            offset,
                            span.row as i32 - offset as i32,
                            col,
                            &render_cell(&line[col], wrapped && col + 1 == line.len()),
                        );
                    }
                });
        }
        true
    }
    pub(super) fn line_bounds(&self) -> (i32, i32) {
        let state = self.shared.state();
        (
            -(state.engine.history_size() as i32),
            i32::from(state.size.rows) - 1,
        )
    }
    pub(super) fn visit_line_cells(
        &self,
        requested_first: i32,
        requested_last: i32,
        mut visitor: impl FnMut((i32, i32, usize), i32, usize, &TerminalRenderCell),
    ) -> (i32, i32, usize) {
        let state = self.shared.state();
        let range = (
            -(state.engine.history_size() as i32),
            i32::from(state.size.rows) - 1,
            usize::from(state.size.cols),
        );
        let mut scratch = Vec::new();
        for line in requested_first.max(range.0)..=requested_last.min(range.1) {
            let wrapped = state.engine.line_wrapped(line);
            state.engine.with_line(line, &mut scratch, |cells| {
                for (col, cell) in cells.iter().enumerate() {
                    visitor(
                        range,
                        line,
                        col,
                        &render_cell(cell, wrapped && col + 1 == cells.len()),
                    );
                }
            });
        }
        range
    }
    pub(super) fn search(&self, query: &str) -> Vec<TermySearchMatch> {
        self.search_with_options(query, TermySearchOptions::default())
    }
    pub(super) fn search_with_options(
        &self,
        query: &str,
        options: TermySearchOptions,
    ) -> Vec<TermySearchMatch> {
        self.search_shared_with_options(query, options)
            .into_iter()
            .map(Into::into)
            .collect()
    }
    pub(super) fn search_shared(&self, query: &str) -> Vec<TermySharedSearchMatch> {
        self.search_shared_with_options(query, TermySearchOptions::default())
    }
    pub(super) fn search_shared_with_options(
        &self,
        query: &str,
        options: TermySearchOptions,
    ) -> Vec<TermySharedSearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        let state = self.shared.state();
        let history = state.engine.history_size() as i32;
        let mut scratch = Vec::new();
        search_lines_shared(
            (-history..i32::from(state.size.rows)).filter_map(|line| {
                state.engine.with_line(line, &mut scratch, |cells| {
                    let mut text = String::with_capacity(cells.len());
                    let mut mapping = SearchLineMapping::default();
                    for (col, cell) in cells.iter().enumerate() {
                        if cell.flags & engine::Cell::WIDE_SPACER != 0 {
                            continue;
                        }
                        let start = text.len();
                        append_search_cell(&mut text, cell);
                        mapping.record_cell(
                            &text,
                            start,
                            col,
                            1 + usize::from(cell.flags & engine::Cell::WIDE != 0),
                        );
                    }
                    text.truncate(text.trim_end().len());
                    ((line + history) as usize, text, mapping)
                })
            }),
            query,
            options,
        )
    }
    pub(super) fn hyperlink_at(&self, row: usize, col: usize) -> Option<DetectedLink> {
        crate::links::hyperlink_at_viewport_cell(&self.shared.state().engine, row, col)
    }

    pub(super) fn link_at(&self, row: usize, col: usize) -> Option<DetectedViewportLink> {
        crate::links::link_at_viewport_cell(&self.shared.state().engine, row, col)
    }

    pub(super) fn scroll_display(&self, delta: i32) -> bool {
        let mut state = self.shared.state();
        let changed = state.engine.scroll_display(delta);
        if changed {
            state.generation = state.generation.wrapping_add(1);
        }
        drop(state);
        if changed {
            self.shared.notify();
        }
        changed
    }
    pub(super) fn scroll_to_bottom(&self) -> bool {
        self.scroll_display(i32::MIN)
    }
    pub(super) fn clear_scrollback(&self) -> bool {
        let mut state = self.shared.state();
        let changed = state.engine.history_size() > 0;
        state.engine.clear_scrollback();
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        if changed {
            self.shared.notify();
        }
        changed
    }
    pub(super) fn scroll_state(&self) -> (usize, usize) {
        let state = self.shared.state();
        (state.engine.display_offset(), state.engine.history_size())
    }
    pub(super) fn cursor_state(&self) -> Option<TerminalCursorState> {
        self.shared.state().cursor()
    }
    pub(super) fn cursor_position(&self) -> (usize, usize) {
        let cursor = self.shared.state().engine.cursor();
        (cursor.col, cursor.row)
    }
    pub(super) fn set_term_options(&self, options: TerminalOptions) {
        let mut state = self.shared.state();
        state.engine.set_options(engine::Options {
            scrollback_history: options
                .scrollback_history
                .min(MAX_TERMINAL_SCROLLBACK_HISTORY),
        });
        state
            .engine
            .set_default_cursor_shape(cursor_shape(options.default_cursor_style));
        state.default_cursor_style = options.default_cursor_style;
        state.generation = state.generation.wrapping_add(1);
        state.force_full_damage = true;
    }
    pub(super) fn set_scrollback_history(&self, history: usize) {
        let style = self.shared.state().default_cursor_style;
        self.set_term_options(TerminalOptions {
            scrollback_history: history,
            default_cursor_style: style,
        });
    }
    pub(super) fn bracketed_paste_mode(&self) -> bool {
        self.shared.state().engine.modes().bracketed_paste
    }
    pub(super) fn alternate_screen_mode(&self) -> bool {
        self.shared.state().engine.alternate_screen()
    }
    pub(super) fn keyboard_mode(&self) -> TerminalKeyboardMode {
        let modes = self.shared.state().engine.modes();
        let bits = modes.kitty_keyboard;
        TerminalKeyboardMode::from_flags(
            modes.application_cursor,
            bits & 1 != 0,
            bits & 2 != 0,
            bits & 4 != 0,
            bits & 8 != 0,
            bits & 16 != 0,
        )
    }
    pub(super) fn mouse_mode(&self) -> TerminalMouseMode {
        let modes = self.shared.state().engine.modes();
        TerminalMouseMode {
            enabled: modes.mouse_tracking != engine::MouseTracking::None,
            report_click: matches!(
                modes.mouse_tracking,
                engine::MouseTracking::Click | engine::MouseTracking::Press
            ),
            report_drag: modes.mouse_tracking == engine::MouseTracking::Drag,
            report_motion: modes.mouse_tracking == engine::MouseTracking::Motion,
            sgr_encoding: matches!(
                modes.mouse_encoding,
                engine::MouseEncoding::Sgr | engine::MouseEncoding::SgrPixels
            ),
            utf8_encoding: modes.mouse_encoding == engine::MouseEncoding::Utf8,
        }
    }
}

#[cfg(test)]
mod tests;
