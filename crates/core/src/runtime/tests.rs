use super::*;
use crate::keyboard::{Keystroke, Modifiers, TerminalKeyEventKind, keystroke_to_input};
use crate::protocol::{
    TerminalClipboardReadRequest, TerminalClipboardReadResult, TerminalClipboardTarget,
    TerminalClipboardWriteRequest, TerminalClipboardWriteResult,
};
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn terminal_size_clamps_absurd_dimensions() {
    let huge = TerminalSize {
        cols: u16::MAX,
        rows: u16::MAX,
        cell_width: 8.0,
        cell_height: 16.0,
    }
    .clamped();
    assert_eq!(huge.cols, MAX_TERMINAL_COLS);
    assert_eq!(huge.rows, 256);
    assert_eq!(
        usize::from(huge.cols) * usize::from(huge.rows),
        crate::terminal_engine::Size::MAX_CELLS
    );
}

#[test]
fn display_terminal_intercepts_and_places_kitty_graphics() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    let initial_revision = terminal.kitty_graphics_revision();
    terminal.feed_output(b"\x1b_Ga=T,f=32,s=1,v=1,i=77,c=2,r=3;AQID/w==\x1b\\");

    let (revision, placements) = terminal.kitty_graphics_snapshot();
    assert!(revision > initial_revision);
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].image_id, 77);
    assert_eq!(placements[0].display_cols, Some(2));
    assert_eq!(placements[0].display_rows, Some(3));
    assert!(placements[0].image.png().starts_with(b"\x89PNG"));

    let cursor = terminal.cursor_position();
    assert_eq!(cursor, (2, 3));
}

#[test]
fn display_terminal_notifier_coalesces_until_events_are_drained() {
    let notifications = Arc::new(AtomicU64::new(0));
    let notification_count = notifications.clone();
    let terminal = Terminal::new_display_with_wakeup_notifier(
        test_terminal_size(),
        None,
        Some(TerminalWakeupNotifier::new(move || {
            notification_count.fetch_add(1, Ordering::Relaxed);
        })),
    );

    terminal.feed_output(b"a");
    terminal.feed_output(b"b");
    assert_eq!(notifications.load(Ordering::Relaxed), 1);

    let mut reply_host = RecordingReplyHost::default();
    let _ = terminal.drain_events(&mut reply_host);
    terminal.feed_output(b"c");
    assert_eq!(notifications.load(Ordering::Relaxed), 2);
}

#[test]
fn remote_screen_identity_matches_cells_during_concurrent_output() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    {
        terminal.feed_output(b"MAIN");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..2_000 {
                    terminal.feed_output(b"\x1b[?1049h\x1b[2J\x1b[HALT");
                    terminal.feed_output(b"\x1b[?1049l");
                }
            });
            for _ in 0..2_000 {
                let state = crate::remote::RemoteState::capture(&terminal);
                let text: String = state
                    .render
                    .cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect();
                let expected = if state.alternate_screen {
                    "ALT"
                } else {
                    "MAIN"
                };
                assert!(
                    text.starts_with(expected),
                    "screen={}, text={text:?}",
                    state.alternate_screen
                );
            }
        });
    }
}

#[test]
fn kitty_command_only_cursor_movement_rejects_old_render_generation() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    let update = terminal.take_render_damage_snapshot();

    terminal.feed_output(b"\x1b_Ga=T,f=32,s=1,v=1,i=177,c=2,r=3;AQID/w==\x1b\\");

    let mut visited = 0;
    assert!(!terminal.visit_viewport_ranges_at_generation(
        update.generation,
        &[crate::TerminalDirtySpan {
            row: 0,
            left_col: 0,
            right_col: 0,
        }],
        |_, _, _, _, _| visited += 1,
    ));
    assert_eq!(visited, 0);
}

#[test]
fn synchronized_update_watchdog_replay_rejects_old_render_generation() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b[?2026h\x1b]4;1;#123456\x07X");
    let old = terminal.take_render_damage_snapshot();
    assert_eq!(terminal.snapshot().cells[0].r#char, ' ');
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while terminal.snapshot().cells[0].r#char != 'X' && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(terminal.snapshot().cells[0].r#char, 'X');
    assert!(!terminal.visit_viewport_ranges_at_generation(
        old.generation,
        &[TerminalDirtySpan {
            row: 0,
            left_col: 0,
            right_col: 0
        }],
        |_, _, _, _, _| {}
    ));
    assert_eq!(
        terminal.palette().indexed[1],
        Some(crate::TerminalColor {
            r: 0x12,
            g: 0x34,
            b: 0x56
        })
    );
}

#[test]
fn display_terminal_scrolls_for_kitty_cursor_advance_at_bottom() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b[4;1H\x1b_Ga=T,f=32,s=1,v=1,i=78,c=2,r=3;AQID/w==\x1b\\");

    assert_eq!(terminal.scroll_state(), (0, 3));
    assert_eq!(terminal.cursor_position(), (2, 3));
    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].viewport_row, 0);
}

#[test]
fn kitty_cursor_advance_tracks_zero_history_screen_scroll() {
    let runtime_config = TerminalRuntimeConfig {
        scrollback_history: 0,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(test_terminal_size(), Some(&runtime_config));
    terminal.feed_output(b"\x1b[4;1H\x1b_Ga=T,f=32,s=1,v=1,i=79,c=2,r=3;AQID/w==\x1b\\");

    assert_eq!(terminal.scroll_state(), (0, 0));
    assert_eq!(terminal.cursor_position(), (2, 3));
    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].viewport_row, 0);
}

#[test]
fn kitty_cursor_advance_tracks_full_history_screen_scroll() {
    let runtime_config = TerminalRuntimeConfig {
        scrollback_history: 2,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(test_terminal_size(), Some(&runtime_config));
    terminal.feed_output(b"\x1b[4;1H\n\n");
    assert_eq!(terminal.scroll_state(), (0, 2));

    terminal.feed_output(b"\x1b_Ga=T,f=32,s=1,v=1,i=81,c=2,r=3;AQID/w==\x1b\\");

    assert_eq!(terminal.scroll_state(), (0, 2));
    assert_eq!(terminal.cursor_position(), (2, 3));
    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].viewport_row, 0);
}

#[test]
fn kitty_cursor_advance_tracks_alternate_screen_scroll() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b[?1049h\x1b[4;1H\x1b_Ga=T,f=32,s=1,v=1,i=80,c=2,r=3;AQID/w==\x1b\\");

    assert!(terminal.alternate_screen_mode());
    assert_eq!(terminal.scroll_state(), (0, 0));
    assert_eq!(terminal.cursor_position(), (2, 3));
    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].viewport_row, 0);
}

#[test]
fn ordinary_newlines_shift_and_remove_alternate_screen_kitty_placement() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal
        .feed_output(b"\x1b[?1049h\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=86,c=2,r=1,C=1;AQID/w==\x1b\\");
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 1);

    terminal.feed_output(b"\x1b[4;1H\n");
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 0);

    terminal.feed_output(b"\n");
    assert!(terminal.kitty_graphics_placements().is_empty());
}

#[test]
fn synchronized_newlines_shift_alternate_screen_kitty_placement() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal
        .feed_output(b"\x1b[?1049h\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=96,c=2,r=1,C=1;AQID/w==\x1b\\");

    terminal.feed_output(b"\x1b[?2026h\x1b[3;1H\n\n\x1b[?2026l");

    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].viewport_row, 0);
}

#[test]
fn ordinary_newlines_shift_and_remove_zero_history_kitty_placement() {
    let runtime_config = TerminalRuntimeConfig {
        scrollback_history: 0,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(test_terminal_size(), Some(&runtime_config));
    terminal.feed_output(b"\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=87,c=2,r=1,C=1;AQID/w==\x1b\\");
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 1);

    terminal.feed_output(b"\x1b[4;1H\n");
    assert_eq!(terminal.scroll_state(), (0, 0));
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 0);

    terminal.feed_output(b"\n");
    assert!(terminal.kitty_graphics_placements().is_empty());
}

#[test]
fn ordinary_wrapped_text_shifts_and_removes_zero_history_kitty_placement() {
    let runtime_config = TerminalRuntimeConfig {
        scrollback_history: 0,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(test_terminal_size(), Some(&runtime_config));
    terminal.feed_output(b"\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=97,c=2,r=1,C=1;AQID/w==\x1b\\");

    terminal.feed_output(b"\x1b[4;32Hab");
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 0);

    terminal.feed_output("\x1b[4;32H界".as_bytes());
    assert!(terminal.kitty_graphics_placements().is_empty());
}

#[test]
fn ordinary_newlines_shift_and_remove_full_history_kitty_placement() {
    let runtime_config = TerminalRuntimeConfig {
        scrollback_history: 1,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(test_terminal_size(), Some(&runtime_config));
    terminal.feed_output(b"\x1b[4;1H\n");
    assert_eq!(terminal.scroll_state(), (0, 1));
    terminal.feed_output(b"\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=88,c=2,r=1,C=1;AQID/w==\x1b\\");
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 1);

    terminal.feed_output(b"\x1b[4;1H\n");
    assert_eq!(terminal.scroll_state(), (0, 1));
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 0);

    terminal.feed_output(b"\n");
    assert!(terminal.kitty_graphics_placements().is_empty());
}

#[test]
fn ordinary_partial_region_scroll_does_not_shift_kitty_placement() {
    let runtime_config = TerminalRuntimeConfig {
        scrollback_history: 0,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(test_terminal_size(), Some(&runtime_config));
    terminal.feed_output(b"\x1b[1;1H\x1b_Ga=T,f=32,s=1,v=1,i=89,c=2,r=1,C=1;AQID/w==\x1b\\");

    terminal.feed_output(b"\x1b[2;3r\x1b[3;1H\n");

    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].viewport_row, 0);
}

#[test]
fn top_anchored_partial_region_scroll_keeps_footer_kitty_placement_fixed() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b[4;1H\x1b_Ga=T,f=32,s=1,v=1,i=90,c=2,r=1,C=1;AQID/w==\x1b\\");
    assert_eq!(terminal.kitty_graphics_placements()[0].viewport_row, 3);

    terminal.feed_output(b"\x1b[1;3r\x1b[3;1H\n");

    assert_eq!(terminal.scroll_state(), (0, 1));
    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].viewport_row, 3);
}

#[test]
fn deccolm_resets_scrolling_region() {
    let mut engine = crate::terminal_engine::Engine::new(
        crate::terminal_engine::Size { cols: 32, rows: 4 },
        Default::default(),
    );
    for sequence in [
        b"\x1b[2;3r\x1b[?3h\x1b[?6h".as_slice(),
        b"\x1b[2;3r\x1b[?3l\x1b[?6h",
    ] {
        engine.feed(sequence);
        assert_eq!(engine.cursor().row, 0);
    }
}

#[test]
fn invalid_omitted_bottom_decstbm_preserves_region() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b[2;3r\x1b[99r\x1b[?6h");
    assert_eq!(terminal.cursor_position(), (0, 1));
}

#[test]
fn kitty_cursor_advance_does_not_scroll_partial_decstbm_region() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal
        .feed_output(b"\x1b[2;3r\x1b[3;1H\x1b_Ga=T,f=32,s=1,v=1,i=82,c=2,r=3;AQID/w==\x1b\\\x1b[r");

    assert_eq!(terminal.scroll_state(), (0, 0));
    assert_eq!(terminal.cursor_position(), (0, 0));
    let placements = terminal.kitty_graphics_placements();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].image_id, 82);
    assert_eq!(placements[0].viewport_row, 2);
}

#[test]
fn primary_kitty_placement_survives_alternate_screen_scroll() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=83,c=2,r=2,C=1;AQID/w==\x1b\\");
    let primary = terminal.kitty_graphics_placements();
    assert_eq!(primary.len(), 1);
    assert_eq!(primary[0].image_id, 83);
    assert_eq!(primary[0].viewport_row, 1);

    terminal.feed_output(b"\x1b[?1049h\x1b[4;1H\x1b_Ga=T,f=32,s=1,v=1,i=84,c=2,r=3;AQID/w==\x1b\\");
    let alternate = terminal.kitty_graphics_placements();
    assert_eq!(alternate.len(), 1);
    assert_eq!(alternate[0].image_id, 84);
    assert_eq!(alternate[0].viewport_row, 0);

    terminal.feed_output(b"\x1b[?1049l");
    let restored_primary = terminal.kitty_graphics_placements();
    assert_eq!(restored_primary.len(), 1);
    assert_eq!(restored_primary[0].image_id, 83);
    assert_eq!(restored_primary[0].viewport_row, 1);

    terminal.feed_output(b"\x1b[?1049h");
    assert!(terminal.kitty_graphics_placements().is_empty());
}

#[test]
fn terminal_reset_clears_kitty_graphics() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b_Ga=T,f=32,s=1,v=1,i=85,c=2,r=2,C=1;AQID/w==\x1b\\");
    assert_eq!(terminal.kitty_graphics_placements().len(), 1);

    terminal.feed_output(b"\x1bc");

    assert!(!terminal.alternate_screen_mode());
    assert!(terminal.kitty_graphics_placements().is_empty());
}

#[test]
fn clear_screen_erases_only_the_active_viewport_graphics() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=98,c=2,r=1,C=1;AQID/w==\x1b\\");

    terminal.feed_output(b"\x1b[J\x1b[1J\x1b[3J");
    assert_eq!(
        terminal.kitty_graphics_placements().len(),
        1,
        "non-ED2 erase commands must not affect graphics"
    );

    terminal
        .feed_output(b"\x1b[?1049h\x1b[2;1H\x1b_Ga=T,f=32,s=1,v=1,i=99,c=2,r=1,C=1;AQID/w==\x1b\\");
    assert_eq!(terminal.kitty_graphics_placements().len(), 1);
    terminal.feed_output(b"\x1b[2J");
    assert!(terminal.kitty_graphics_placements().is_empty());

    terminal.feed_output(b"\x1b[?1049l");
    assert_eq!(
        terminal.kitty_graphics_placements().len(),
        1,
        "clearing the alternate viewport must preserve primary graphics"
    );

    terminal.feed_output(b"\x1b[H\x1b[2J");
    assert!(terminal.kitty_graphics_placements().is_empty());
}

#[test]
fn kitty_cursor_advance_caps_linefeeds_to_screen_height() {
    let size = test_terminal_size();
    let mut engine = crate::terminal_engine::Engine::new(
        crate::terminal_engine::Size {
            cols: usize::from(size.cols),
            rows: usize::from(size.rows),
        },
        Default::default(),
    );
    engine.feed(b"\x1b[4;1H");
    engine.feed(b"\x1b_Ga=T,f=32,s=1,v=1,i=77,c=4294967295,r=4294967295;AQID/w==\x1b\\");
    assert_eq!(engine.history_size(), usize::from(size.rows));
    assert_eq!(engine.cursor().col, usize::from(size.cols) - 1);
    assert_eq!(engine.cursor().row, usize::from(size.rows) - 1);
}

#[test]
fn terminal_size_clamp_leaves_realistic_dimensions_untouched() {
    let clamped = TerminalSize {
        cols: 200,
        rows: 60,
        cell_width: 9.0,
        cell_height: 18.0,
    }
    .clamped();
    assert_eq!(clamped.cols, 200);
    assert_eq!(clamped.rows, 60);
}

#[test]
fn cell_metric_queries_round_and_never_report_zero() {
    let mut engine = crate::terminal_engine::Engine::new(Default::default(), Default::default());
    engine.set_cell_pixels(9.6, 18.4);
    engine.feed(b"\x1b[16t");
    let mut replies = Vec::new();
    engine.drain_replies(&mut replies);
    assert_eq!(replies, b"\x1b[6;18;10t");
    replies.clear();
    engine.set_cell_pixels(0.2, 0.0);
    engine.feed(b"\x1b[16t");
    engine.drain_replies(&mut replies);
    assert_eq!(replies, b"\x1b[6;1;1t");
}

#[test]
fn identical_terminal_resize_does_not_redamage_the_grid() {
    let size = TerminalSize {
        cols: 80,
        rows: 24,
        cell_width: 9.0,
        cell_height: 18.0,
    };
    let mut terminal = Terminal::new_display(size, None);
    assert_eq!(
        terminal.take_damage_snapshot(),
        TerminalDamageSnapshot::Full
    );
    let stable_damage = terminal.take_damage_snapshot();
    assert_eq!(terminal.take_damage_snapshot(), stable_damage);

    terminal.resize(size);

    assert_eq!(terminal.size(), size);
    assert_eq!(terminal.take_damage_snapshot(), stable_damage);
}

#[test]
fn terminal_size_clamp_floors_zero_dimensions_at_one() {
    let empty = TerminalSize {
        cols: 0,
        rows: 0,
        cell_width: 9.0,
        cell_height: 18.0,
    }
    .clamped();
    assert_eq!(empty.cols, 1);
    assert_eq!(empty.rows, 1);
}

#[test]
fn terminal_options_clamp_scrollback_history() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.set_term_options(TerminalOptions {
        scrollback_history: 10_000_000,
        default_cursor_style: TerminalCursorStyle::Block,
    });
    terminal.feed_output(
        &(0..MAX_TERMINAL_SCROLLBACK_HISTORY.saturating_add(100))
            .map(|_| "x\r\n")
            .collect::<String>()
            .into_bytes(),
    );
    assert!(terminal.scroll_state().1 <= MAX_TERMINAL_SCROLLBACK_HISTORY);
}

#[test]
fn terminal_options_preserve_in_range_scrollback_history() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.set_term_options(TerminalOptions {
        scrollback_history: 5,
        default_cursor_style: TerminalCursorStyle::Block,
    });
    terminal.feed_output(&(0..20).map(|_| "x\r\n").collect::<String>().into_bytes());
    assert_eq!(terminal.scroll_state().1, 5);
}

fn test_terminal_size() -> TerminalSize {
    TerminalSize {
        cols: 32,
        rows: 4,
        cell_width: 9.0,
        cell_height: 18.0,
    }
}

fn cursor_after_bytes(input: &[u8]) -> (usize, i32) {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(input);
    let (col, row) = terminal.cursor_position();
    (col, row as i32)
}

fn cursor_state_after_bytes(
    input: &[u8],
    runtime_config: TerminalRuntimeConfig,
) -> Option<TerminalCursorState> {
    let terminal = Terminal::new_display(test_terminal_size(), Some(&runtime_config));
    terminal.feed_output(input);
    terminal.cursor_state()
}

fn cursor_position_after_bytes(input: &[u8]) -> (usize, usize) {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(input);
    terminal.cursor_position()
}

fn mouse_mode_after_bytes(input: &[u8]) -> crate::mouse_protocol::TerminalMouseMode {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(input);
    terminal.mouse_mode()
}

fn keystroke(key: &str, modifiers: Modifiers) -> Keystroke {
    Keystroke {
        modifiers,
        key: key.to_string(),
        key_char: None,
    }
}

fn press_mode() -> TerminalKeyboardMode {
    TerminalKeyboardMode::default()
}

#[derive(Default)]
struct RecordingReplyHost {
    clipboard_text: Option<String>,
    requested_targets: Vec<TerminalClipboardTarget>,
    kitty_reads: Vec<TerminalClipboardReadRequest>,
    kitty_writes: Vec<TerminalClipboardWriteRequest>,
    protocol_replies: Vec<u8>,
}

impl TerminalReplyHost for RecordingReplyHost {
    fn load_clipboard(&mut self, target: TerminalClipboardTarget) -> Option<String> {
        self.requested_targets.push(target);
        self.clipboard_text.clone()
    }

    fn protocol_reply(&mut self, bytes: &[u8]) {
        self.protocol_replies.extend_from_slice(bytes);
    }

    fn read_clipboard(
        &mut self,
        request: TerminalClipboardReadRequest,
    ) -> TerminalClipboardReadResult {
        self.kitty_reads.push(request);
        TerminalClipboardReadResult::Success {
            available_formats: vec!["text/plain".to_string(), "image/png".to_string()],
            contents: Vec::new(),
            remember_permission: false,
        }
    }

    fn write_clipboard(
        &mut self,
        request: TerminalClipboardWriteRequest,
    ) -> TerminalClipboardWriteResult {
        self.kitty_writes.push(request);
        TerminalClipboardWriteResult::Success {
            remember_permission: false,
        }
    }
}

fn assert_kitty_clipboard_runtime(terminal: &Terminal) {
    terminal.feed_output(
        b"\x1b[?5522h\
              \x1b]5522;type=read:id=list;Lg==\x1b\\\
              \x1b]5522;type=write:id=write\x1b\\\
              \x1b]5522;type=walias:mime=dGV4dC9wbGFpbg==;dGV4dC91dGY4\x1b\\\
              \x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;aGVsbG8=\x1b\\\
              \x1b]5522;type=wdata;\x1b\\",
    );

    let mut host = RecordingReplyHost::default();
    let (_, has_more) = terminal.drain_events(&mut host);

    assert!(!has_more);
    assert!(terminal.kitty_clipboard_paste_events_enabled());
    assert_eq!(host.kitty_reads.len(), 1);
    assert!(host.kitty_reads[0].list_available);
    assert_eq!(host.kitty_writes.len(), 1);
    assert_eq!(host.kitty_writes[0].contents.len(), 2);
    assert_eq!(host.kitty_writes[0].contents[0].mime_type, "text/utf8");
    assert_eq!(host.kitty_writes[0].contents[1].data, b"hello");
    let replies = String::from_utf8_lossy(&host.protocol_replies);
    assert!(replies.contains("type=read:status=OK:id=list"));
    assert!(replies.contains("type=read:status=DONE:id=list"));
    assert!(replies.contains("type=write:status=DONE:id=write"));

    host.protocol_replies.clear();
    assert!(terminal.send_kitty_clipboard_paste_event(
        crate::TerminalClipboardLocation::Clipboard,
        &["text/plain".to_string()],
    ));
    let (_, has_more) = terminal.drain_events(&mut host);
    assert!(!has_more);
    let replies = String::from_utf8_lossy(&host.protocol_replies);
    assert!(replies.contains("type=read:status=OK"));
    assert!(replies.contains("type=read:status=DONE"));
}

#[test]
fn kitty_clipboard_routes_through_native_core() {
    assert_kitty_clipboard_runtime(&Terminal::new_display(test_terminal_size(), None));
}

#[test]
fn normalize_working_directory_candidate_preserves_relative_paths() {
    assert_eq!(
        normalize_working_directory_candidate(Some(" crates/cli ")).as_deref(),
        Some("crates/cli")
    );
}

#[test]
fn normalize_working_directory_candidate_rejects_control_characters() {
    assert_eq!(
        normalize_working_directory_candidate(Some("/tmp/project\nrun-shell")),
        None
    );
}

#[test]
fn resolve_launch_working_directory_falls_back_when_configured_path_is_invalid() {
    let fallback = std::env::current_dir().expect("current dir");
    let resolved = resolve_launch_working_directory(
        Some("/definitely/not/a/real/termy/path"),
        WorkingDirFallback::Process,
    )
    .expect("fallback path");
    assert_eq!(resolved, fallback);
}

#[test]
fn normalize_working_directory_candidate_expands_home_directory() {
    let expected = user_home_dir()
        .expect("home dir")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        normalize_working_directory_candidate(Some("~")).as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn drain_events_replays_protocol_queries_and_collects_terminal_events() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b]10;#123456\x07\x1b[c\x1b[18t\x1b]52;p;?\x1b\\\x1b]10;?\x1b\\\x1b]2;shell title\x07\x1b]52;c;c3RvcmVkIHRleHQ=\x07");
    let mut host = RecordingReplyHost {
        clipboard_text: Some("payload".to_owned()),
        ..Default::default()
    };
    let (events, has_more) = terminal.drain_events(&mut host);
    assert!(!has_more);
    let replies = String::from_utf8_lossy(&host.protocol_replies);
    assert!(replies.contains("\x1b[?62;22c"));
    assert!(replies.contains("\x1b[8;4;32t"));
    assert!(replies.contains("\x1b]52;p;cGF5bG9hZA==\x1b\\"));
    assert!(replies.contains("\x1b]10;rgb:1212/3434/5656\x1b\\"));
    assert_eq!(
        host.requested_targets,
        vec![TerminalClipboardTarget::Selection]
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Title(title) if title == "shell title"))
    );
    assert!(events.iter().any(
        |event| matches!(event, TerminalEvent::ClipboardStore(text) if text == "stored text")
    ));
}

#[test]
fn drain_events_includes_custom_osc_progress_events() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"\x1b]9;4;3\x07");
    let (events, has_more) = terminal.drain_events(&mut RecordingReplyHost::default());
    assert!(!has_more);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Progress(ProgressState::Indeterminate)))
    );
}

#[test]
fn drain_events_coalesces_consecutive_wakeups() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    for _ in 0..128 {
        terminal.feed_output(b"x");
    }
    terminal.feed_output(b"\x1b]2;ready\x07");
    let (events, has_more) = terminal.drain_events(&mut RecordingReplyHost::default());
    assert!(!has_more);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TerminalEvent::Wakeup))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Title(title) if title == "ready"))
    );
}

#[test]
fn mouse_mode_detects_click_reporting() {
    let mode = mouse_mode_after_bytes(b"\x1b[?1000h");
    assert!(mode.enabled);
    assert!(mode.report_click);
    assert!(!mode.report_drag);
    assert!(!mode.report_motion);
}

#[test]
fn mouse_mode_detects_drag_reporting() {
    let mode = mouse_mode_after_bytes(b"\x1b[?1002h");
    assert!(mode.enabled);
    assert!(mode.report_drag);
    assert!(!mode.report_motion);
}

#[test]
fn mouse_mode_detects_motion_reporting() {
    let mode = mouse_mode_after_bytes(b"\x1b[?1003h");
    assert!(mode.enabled);
    assert!(mode.report_motion);
}

#[test]
fn mouse_mode_detects_sgr_encoding() {
    let mode = mouse_mode_after_bytes(b"\x1b[?1006h");
    assert!(mode.sgr_encoding);
}

#[test]
fn mouse_mode_detects_utf8_reporting() {
    let mode = mouse_mode_after_bytes(b"\x1b[?1005h");
    assert!(mode.utf8_encoding);
}

#[test]
fn terminal_damage_snapshot_is_full_for_new_terminal() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    assert!(matches!(
        terminal.take_damage_snapshot(),
        TerminalDamageSnapshot::Full
    ));
}

#[test]
fn terminal_damage_snapshot_resets_damage_after_read() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    let _ = terminal.take_damage_snapshot();
    let second = terminal.take_damage_snapshot();
    let third = terminal.take_damage_snapshot();
    assert!(matches!(second, TerminalDamageSnapshot::Partial(_)));
    assert_eq!(second, third);
}

#[test]
fn terminal_damage_snapshot_returns_partial_spans_for_output() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    let _ = terminal.take_damage_snapshot();
    terminal.feed_output(b"abc");
    assert!(matches!(
        terminal.take_damage_snapshot(),
        TerminalDamageSnapshot::Partial(spans) if !spans.is_empty()
    ));
}

#[test]
fn terminal_damage_snapshot_while_scrolled_returns_empty_partial_without_damage() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    let _ = terminal.take_damage_snapshot();
    terminal.feed_output(b"1\n2\n3\n4\n5\n6\n");
    let _ = terminal.take_damage_snapshot();

    assert!(terminal.scroll_display(1));
    assert!(terminal.scroll_state().0 > 0);

    assert!(matches!(
        terminal.take_damage_snapshot(),
        TerminalDamageSnapshot::Full
    ));
    assert_eq!(
        terminal.take_damage_snapshot(),
        TerminalDamageSnapshot::Partial(Vec::new())
    );
}

#[test]
fn terminal_damage_snapshot_while_scrolled_maps_damage_to_viewport_rows() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    let _ = terminal.take_damage_snapshot();
    terminal.feed_output(b"1\n2\n3\n4\n5\n6\n");
    let _ = terminal.take_damage_snapshot();

    assert!(terminal.scroll_display(1));
    let _ = terminal.take_damage_snapshot();
    let _ = terminal.take_damage_snapshot();

    terminal.feed_output(b"\x1b[1;1H");
    match terminal.take_damage_snapshot() {
        TerminalDamageSnapshot::Partial(spans) => {
            assert!(spans.iter().any(|span| span.row == 1), "spans: {spans:?}");
            assert!(spans.iter().all(|span| span.row < 4), "spans: {spans:?}");
        }
        TerminalDamageSnapshot::Full => {
            panic!("visible damage while scrolled should stay partial")
        }
    }
}

#[test]
fn terminal_damage_snapshot_while_scrolled_drops_damage_below_viewport() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    let _ = terminal.take_damage_snapshot();
    terminal.feed_output(b"1\n2\n3\n4\n5\n6\n");
    let _ = terminal.take_damage_snapshot();

    assert!(terminal.scroll_display(3));
    let _ = terminal.take_damage_snapshot();
    let _ = terminal.take_damage_snapshot();

    terminal.feed_output(b"x");
    match terminal.take_damage_snapshot() {
        TerminalDamageSnapshot::Partial(spans) => {
            assert!(spans.iter().all(|span| span.row < 4), "spans: {spans:?}");
        }
        TerminalDamageSnapshot::Full => {
            panic!("invisible damage while scrolled should stay partial")
        }
    }
}

#[test]
fn rich_render_read_preserves_text_colors_attributes_and_metadata() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(
        "\x1b[38;2;1;2;3;48;5;4;4:3;58:2::7:8:9;1;2;3;7;8;9me\u{301}\x1b[0m 界".as_bytes(),
    );

    let read = terminal.render_read(true);
    assert_eq!((read.metadata.cols, read.metadata.rows), (32, 4));
    assert_eq!(read.metadata.generation, read.update.generation);
    assert_eq!(read.metadata.palette_revision, read.update.palette_revision);
    assert_eq!(read.metadata.palette_revision, read.palette.revision);
    assert!(matches!(read.update.damage, TerminalDamageSnapshot::Full));
    assert!(read.update.scrolls.is_empty());

    let cell = &read.cells[0];
    assert_eq!(cell.text, "e\u{301}");
    assert_eq!(
        cell.foreground,
        crate::TerminalRenderColor::Rgb(crate::TerminalColor { r: 1, g: 2, b: 3 })
    );
    assert_eq!(cell.background, crate::TerminalRenderColor::Indexed(4));
    assert_eq!(
        cell.underline_color,
        Some(crate::TerminalRenderColor::Rgb(crate::TerminalColor {
            r: 7,
            g: 8,
            b: 9,
        }))
    );
    assert_eq!(cell.underline_style, crate::TerminalUnderlineStyle::Curly);
    assert!(cell.bold);
    assert!(cell.dim);
    assert!(cell.italic);
    assert!(cell.inverse);
    assert!(cell.hidden);
    assert!(cell.strikethrough);
    assert!(read.cells.iter().any(|cell| cell.wide_character_spacer));
}

#[test]
fn viewport_visitors_allow_reentrant_terminal_reads() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"abc");

    let mut viewport_reentered = false;
    terminal.visit_viewport_cells(|_, _, _, _| {
        if !viewport_reentered {
            viewport_reentered = true;
            assert_eq!(terminal.line_bounds().1, 3);
        }
    });
    assert!(viewport_reentered);

    let update = terminal.take_render_damage_snapshot();
    let spans = match &update.damage {
        TerminalDamageSnapshot::Partial(spans) if !spans.is_empty() => spans.clone(),
        TerminalDamageSnapshot::Full | TerminalDamageSnapshot::Partial(_) => {
            vec![crate::TerminalDirtySpan {
                row: 0,
                left_col: 0,
                right_col: 0,
            }]
        }
    };
    let mut range_reentered = false;
    assert!(terminal.visit_viewport_ranges_at_generation(
        update.generation,
        &spans,
        |_, _, _, _, _| {
            if !range_reentered {
                range_reentered = true;
                let _ = terminal.palette();
            }
        },
    ));
    assert!(range_reentered);
}

#[test]
fn locked_viewport_visitors_stream_cells_and_reject_stale_ranges() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"abc");

    let mut first_row = String::new();
    let mut visited = 0;
    let metadata = terminal.visit_viewport_cells_locked(|viewport_row, _, _, cell| {
        visited += 1;
        if viewport_row == 0 {
            first_row.push_str(&cell.text);
        }
    });
    assert!(first_row.starts_with("abc"));
    assert_eq!(
        visited,
        usize::from(test_terminal_size().cols) * usize::from(test_terminal_size().rows)
    );

    let spans = [crate::TerminalDirtySpan {
        row: 0,
        left_col: 0,
        right_col: 2,
    }];
    let mut selected = String::new();
    assert!(terminal.visit_viewport_ranges_locked_at_generation(
        metadata.generation,
        &spans,
        |_, _, _, _, cell| selected.push_str(&cell.text),
    ));
    assert_eq!(selected, "abc");

    terminal.feed_output(b"d");
    let mut stale_visited = false;
    assert!(!terminal.visit_viewport_ranges_locked_at_generation(
        metadata.generation,
        &spans,
        |_, _, _, _, _| stale_visited = true,
    ));
    assert!(!stale_visited);
}

#[test]
fn rich_line_visitor_streams_cells_in_buffer_order() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"abc\r\ndef");

    let mut visited = Vec::new();
    let bounds = terminal.visit_line_cells(0, 1, |range, line, col, cell| {
        if col < 3 {
            visited.push((range, line, col, cell.text.to_string()));
        }
    });

    assert_eq!(bounds, (0, 3, 32));
    assert_eq!(
        visited,
        vec![
            ((0, 3, 32), 0, 0, "a".to_string()),
            ((0, 3, 32), 0, 1, "b".to_string()),
            ((0, 3, 32), 0, 2, "c".to_string()),
            ((0, 3, 32), 1, 0, "d".to_string()),
            ((0, 3, 32), 1, 1, "e".to_string()),
            ((0, 3, 32), 1, 2, "f".to_string()),
        ]
    );
}

#[test]
fn rich_render_cell_keeps_common_text_inline() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(b"plain");

    let read = terminal.render_read(true);
    assert!(read.cells.iter().all(|cell| !cell.text.is_heap_allocated()));

    terminal.feed_output("\rcombined e\u{301}".as_bytes());
    let read = terminal.render_read(true);
    assert!(
        read.cells
            .iter()
            .filter(|cell| !cell.text.is_empty())
            .all(|cell| !cell.text.is_heap_allocated())
    );
}

#[test]
fn rich_render_read_reports_wrap_generation_palette_and_partial_cells() {
    let terminal = Terminal::new_display(
        TerminalSize {
            cols: 4,
            rows: 2,
            ..test_terminal_size()
        },
        None,
    );
    let initial = terminal.render_read(true);
    let _ = terminal.take_render_damage_snapshot();
    terminal.feed_output(b"abcde");

    let update = terminal.take_render_damage_snapshot();
    assert!(update.generation > initial.metadata.generation);
    assert!(matches!(
        update.damage,
        TerminalDamageSnapshot::Partial(ref spans) if !spans.is_empty()
    ));
    let spans = match &update.damage {
        TerminalDamageSnapshot::Partial(spans) => spans,
        TerminalDamageSnapshot::Full => unreachable!(),
    };
    let mut visited = Vec::new();
    assert!(terminal.visit_viewport_ranges_at_generation(
        update.generation,
        spans,
        |row, display_offset, line, col, cell| {
            visited.push((row, display_offset, line, col, cell.text.clone()));
        },
    ));
    assert!(!visited.is_empty());

    terminal.feed_output(b"\x1b]4;1;#123456\x07");
    let read = terminal.render_read(true);
    assert!(read.cells[3].line_wrapped);
    assert_eq!(
        read.palette.indexed[1],
        Some(crate::TerminalColor {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        })
    );
    assert!(read.metadata.palette_revision > initial.metadata.palette_revision);
}

#[test]
fn core_cursor_advance_matches_for_ascii_and_starship_glyph() {
    let ascii = cursor_after_bytes(b"> ");
    let starship = cursor_after_bytes("❯ ".as_bytes());
    assert_eq!(ascii, starship);
}

#[test]
fn core_cursor_advance_ignores_ansi_sequences_for_ascii_and_starship_glyph() {
    let ascii = cursor_after_bytes(b"\x1b[1;32m>\x1b[0m ");
    let starship = cursor_after_bytes("\x1b[1;32m❯\x1b[0m ".as_bytes());
    assert_eq!(ascii, starship);
}

#[test]
fn core_cursor_advance_matches_after_osc_title_with_bel_terminator() {
    let ascii = cursor_after_bytes(b"\x1b]2;termy:tab:prompt:/tmp\x07> ");
    let starship = cursor_after_bytes("\x1b]2;termy:tab:prompt:/tmp\x07❯ ".as_bytes());
    assert_eq!(ascii, starship);
}

#[test]
fn core_cursor_advance_matches_after_osc_title_with_st_terminator() {
    let ascii = cursor_after_bytes(b"\x1b]2;termy:tab:prompt:/tmp\x1b\\> ");
    let starship = cursor_after_bytes("\x1b]2;termy:tab:prompt:/tmp\x1b\\❯ ".as_bytes());
    assert_eq!(ascii, starship);
}

#[test]
fn cursor_state_hides_and_restores_with_terminal_visibility_sequences() {
    let hidden = cursor_state_after_bytes(b"prompt\x1b[?25l", TerminalRuntimeConfig::default());
    assert_eq!(hidden, None);

    let restored = cursor_state_after_bytes(
        b"prompt\x1b[?25l\x1b[?25h",
        TerminalRuntimeConfig::default(),
    );
    assert_eq!(
        restored,
        Some(TerminalCursorState {
            col: 6,
            row: 0,
            style: TerminalCursorStyle::Block,
        })
    );
}

#[test]
fn cursor_position_remains_available_when_terminal_hides_cursor() {
    assert_eq!(cursor_position_after_bytes(b"prompt\x1b[?25l"), (6, 0));
}

#[test]
fn cursor_state_maps_terminal_requested_shapes_to_supported_renderer_styles() {
    let block = cursor_state_after_bytes(
        b"\x1b[2 q",
        TerminalRuntimeConfig {
            default_cursor_style: TerminalCursorStyle::Line,
            ..TerminalRuntimeConfig::default()
        },
    );
    assert_eq!(
        block,
        Some(TerminalCursorState {
            col: 0,
            row: 0,
            style: TerminalCursorStyle::Block,
        })
    );

    let underline = cursor_state_after_bytes(b"\x1b[4 q", TerminalRuntimeConfig::default());
    assert_eq!(
        underline,
        Some(TerminalCursorState {
            col: 0,
            row: 0,
            style: TerminalCursorStyle::Line,
        })
    );

    let beam = cursor_state_after_bytes(b"\x1b[6 q", TerminalRuntimeConfig::default());
    assert_eq!(
        beam,
        Some(TerminalCursorState {
            col: 0,
            row: 0,
            style: TerminalCursorStyle::Line,
        })
    );
}

#[test]
fn applying_runtime_options_preserves_default_cursor_style_when_scrollback_changes() {
    let size = test_terminal_size();
    let initial = TerminalRuntimeConfig {
        scrollback_history: 256,
        default_cursor_style: TerminalCursorStyle::Line,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(size, Some(&initial));

    let updated = TerminalRuntimeConfig {
        scrollback_history: 8,
        ..initial
    };
    terminal.set_term_options(updated.term_options());
    let output = (0..80)
        .map(|index| format!("line-{index}\r\n"))
        .collect::<String>();
    terminal.feed_output(output.as_bytes());

    assert_eq!(terminal.scroll_state().1, 8);
    assert_eq!(
        terminal.cursor_state().unwrap().style,
        TerminalCursorStyle::Line
    );
}

#[test]
fn shrinking_scrollback_trims_history_and_keeps_terminal_usable() {
    let size = test_terminal_size();
    let initial = TerminalRuntimeConfig {
        scrollback_history: 256,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(size, Some(&initial));

    let output = (0..300)
        .map(|index| format!("line-{index}\r\n"))
        .collect::<String>();
    terminal.feed_output(output.as_bytes());
    assert_eq!(terminal.scroll_state().1, 256);

    // Shrink (the inactive-tab path), which must also trim the raw buffer.
    let inactive = TerminalRuntimeConfig {
        scrollback_history: 16,
        ..initial.clone()
    };
    terminal.set_term_options(inactive.term_options());
    assert_eq!(terminal.scroll_state().1, 16);

    // Grow back (tab reactivated) and keep scrolling: storage must regrow.
    terminal.set_term_options(initial.term_options());
    terminal.feed_output(output.as_bytes());
    assert_eq!(terminal.scroll_state().1, 256);
}

#[test]
fn applying_runtime_options_preserves_scrollback_when_cursor_style_changes() {
    let size = test_terminal_size();
    let initial = TerminalRuntimeConfig {
        scrollback_history: 8,
        ..TerminalRuntimeConfig::default()
    };
    let terminal = Terminal::new_display(size, Some(&initial));

    let updated = TerminalRuntimeConfig {
        default_cursor_style: TerminalCursorStyle::Line,
        ..initial
    };
    terminal.set_term_options(updated.term_options());
    let output = (0..80)
        .map(|index| format!("line-{index}\r\n"))
        .collect::<String>();
    terminal.feed_output(output.as_bytes());

    assert_eq!(terminal.scroll_state().1, 8);
    assert_eq!(
        terminal.cursor_state().unwrap().style,
        TerminalCursorStyle::Line
    );
}

#[cfg(unix)]
#[test]
fn locale_override_plan_forces_lc_ctype_when_no_utf8_and_no_lc_all() {
    assert_eq!(
        super::utf8_locale_override_plan(None, Some("C"), Some("")),
        super::Utf8LocaleOverridePlan::LcCtypeOnly
    );
}

#[cfg(unix)]
#[test]
fn locale_override_plan_forces_lc_all_when_lc_all_is_non_utf8() {
    assert_eq!(
        super::utf8_locale_override_plan(Some("C"), Some("C"), Some("")),
        super::Utf8LocaleOverridePlan::LcAllAndLcCtype
    );
}

#[cfg(unix)]
#[test]
fn locale_override_plan_skips_when_utf8_present() {
    assert_eq!(
        super::utf8_locale_override_plan(Some("en_US.UTF-8"), Some("C"), Some("")),
        super::Utf8LocaleOverridePlan::None
    );
}

#[cfg(unix)]
#[test]
fn locale_override_plan_prefers_lc_all_over_lang() {
    assert_eq!(
        super::utf8_locale_override_plan(Some("fr_FR.ISO8859-1"), Some("C"), Some("en_US.UTF-8")),
        super::Utf8LocaleOverridePlan::LcAllAndLcCtype
    );
}

#[cfg(unix)]
#[test]
fn locale_override_plan_does_not_skip_for_utf8_substring_false_positive() {
    assert_eq!(
        super::utf8_locale_override_plan(Some("en_US.fakeutf8"), Some("C"), Some("")),
        super::Utf8LocaleOverridePlan::LcAllAndLcCtype
    );
}

#[cfg(unix)]
#[test]
fn locale_override_plan_skips_for_utf8_with_modifier() {
    assert_eq!(
        super::utf8_locale_override_plan(Some("en_US.UTF-8@variant"), Some("C"), Some("")),
        super::Utf8LocaleOverridePlan::None
    );
}

#[cfg(unix)]
#[test]
fn preferred_utf8_locale_preserves_lang_region_from_lc_all() {
    assert_eq!(
        super::preferred_utf8_locale(Some("fr_FR.ISO8859-1"), Some("C"), Some("en_US.ISO8859-1")),
        "fr_FR.UTF-8"
    );
}

#[cfg(unix)]
#[test]
fn preferred_utf8_locale_preserves_locale_modifier() {
    assert_eq!(
        super::preferred_utf8_locale(None, Some("sr_RS@latin"), Some("")),
        "sr_RS.UTF-8@latin"
    );
}

#[cfg(unix)]
#[test]
fn preferred_utf8_locale_falls_back_for_c_or_posix() {
    assert_eq!(
        super::preferred_utf8_locale(Some("C"), Some("POSIX"), Some("")),
        crate::locale::DEFAULT_UTF8_LOCALE
    );
}

mod input_and_launch;
mod search_tests;

#[test]
fn program_status_reports_are_coalesced_and_clear_through_public_events() {
    let terminal = Terminal::new_display(test_terminal_size(), None);
    terminal.feed_output(
        b"\x1b]7501;?\x07\x1b]7501;state=working:app=cargo\x07\x1b]7501;state=done:msg=SGk=\x07",
    );
    let mut host = RecordingReplyHost::default();
    let (events, _) = terminal.drain_events(&mut host);
    assert_eq!(host.protocol_replies, b"\x1b]7501;?\x1b\\");
    let snapshots: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TerminalEvent::ProgramStatus(records) => Some(records),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0][0].state, crate::ProgramState::Done);
    assert_eq!(snapshots[0][0].app, None);
    terminal.feed_output(b"\x1b]133;A\x07");
    let (events, _) = terminal.drain_events(&mut host);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, TerminalEvent::ProgramStatus(_)))
    );
    terminal.feed_output(b"\x1bc");
    let (events, _) = terminal.drain_events(&mut host);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TerminalEvent::ProgramStatus(records) if records.is_empty()))
    );
}
