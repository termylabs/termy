use super::*;
use std::time::Duration;

fn display(cols: u16, rows: u16) -> CustomBackend {
    CustomBackend::new_display(
        TerminalSize {
            cols,
            rows,
            ..TerminalSize::default()
        },
        None,
    )
}

#[test]
fn facade_preserves_wide_combining_and_history_cells() {
    let terminal = display(6, 2);
    terminal.feed_output("a界e\u{301}\r\nsecond\r\nlast".as_bytes());
    assert_eq!(terminal.scroll_state().1, 1);
    let mut observed = Vec::new();
    terminal.visit_line_cells(-1, -1, |_, _, _, cell| observed.push(cell.clone()));
    assert_eq!(observed[1].text.as_str(), "界");
    assert!(observed[2].wide_character_spacer);
    assert_eq!(observed[3].text.as_str(), "e\u{301}");
    assert_eq!(terminal.search("last")[0].row, 2);
}

#[test]
fn quiet_history_compacts_without_changing_the_frame_or_generation() {
    let terminal = display(120, 4);
    for _ in 0..40 {
        terminal.feed_output("你好 👩🏽‍💻 a short history line\r\n".as_bytes());
    }
    let generation = terminal.shared.state().generation;
    assert!(terminal.shared.state().engine.needs_history_compaction());
    let deadline = Instant::now() + Duration::from_secs(2);
    while terminal.shared.state().engine.needs_history_compaction() {
        assert!(Instant::now() < deadline, "idle history timer did not run");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(terminal.shared.state().generation, generation);
    let mut cells = Vec::new();
    terminal.visit_line_cells(-1, -1, |_, _, _, cell| cells.push(cell.clone()));
    assert_eq!(cells[5].text.as_str(), "👩🏽‍💻");
    terminal.feed_output(b"another line\r\n");
    assert!(terminal.shared.state().engine.needs_history_compaction());
}

#[test]
fn generation_rejects_cells_changed_since_damage_read() {
    let terminal = display(6, 2);
    terminal.take_render_damage_snapshot();
    terminal.feed_output(b"x");
    let update = terminal.take_render_damage_snapshot();
    let TerminalDamageSnapshot::Partial(spans) = update.damage else {
        panic!("incremental damage");
    };
    let mut visited = 0;
    assert!(terminal.visit_viewport_ranges_at_generation(
        update.generation,
        &spans,
        |_, _, _, _, _| visited += 1
    ));
    assert!(visited > 0);
    terminal.feed_output(b"y");
    assert!(!terminal.visit_viewport_ranges_at_generation(
        update.generation,
        &spans,
        |_, _, _, _, _| panic!("stale read")
    ));
}

#[test]
fn forced_read_consumes_full_damage_once() {
    let terminal = display(6, 2);
    assert!(matches!(
        terminal.render_read(true).update.damage,
        TerminalDamageSnapshot::Full
    ));
    assert_eq!(
        terminal.take_damage_snapshot(),
        TerminalDamageSnapshot::Partial(Vec::new())
    );
}

#[test]
fn display_routes_cursor_and_clipboard_queries() {
    struct Host {
        replies: Vec<u8>,
    }
    impl TerminalReplyHost for Host {
        fn load_clipboard(&mut self, _: TerminalClipboardTarget) -> Option<String> {
            Some("hello".to_owned())
        }
        fn protocol_reply(&mut self, bytes: &[u8]) {
            self.replies.extend_from_slice(bytes);
        }
    }
    let terminal = display(6, 2);
    terminal.feed_output(b"hi\x1b[6n\x1b]52;c;?\x1b\\");
    let mut host = Host {
        replies: Vec::new(),
    };
    terminal.drain_events(&mut host);
    assert_eq!(host.replies, b"\x1b[1;3R\x1b]52;c;aGVsbG8=\x1b\\");
}

#[test]
fn event_payload_budget_counts_all_osc_payloads_and_reopens_after_drain() {
    const PAYLOAD_BYTES: usize = 64 * 1024;
    let terminal = display(6, 2);
    let packet_body = vec![b'x'; PAYLOAD_BYTES];
    {
        let mut state = terminal.shared.state();
        for index in 0..512 {
            // Spare capacity must count too: tiny strings can retain a
            // large allocation after an upstream parser trims their text.
            let mut text = String::with_capacity(PAYLOAD_BYTES);
            text.push('x');
            let event = match index % 5 {
                0 => PendingEvent::Terminal(TerminalEvent::Title(text)),
                1 => PendingEvent::Terminal(TerminalEvent::WorkingDirectory(text)),
                2 => PendingEvent::Terminal(TerminalEvent::ClipboardStore(text)),
                3 => PendingEvent::ClipboardLoad(text),
                _ => PendingEvent::Kitty(KittyClipboardOsc::from_body(
                    &packet_body,
                    crate::KittyClipboardOscTerminator::StringTerminator,
                )),
            };
            state.queue(event);
            assert!(state.event_payload_bytes <= MAX_EVENT_PAYLOAD_BYTES);
        }
        assert!(
            state.events.len() < 512,
            "overload must discard excess payloads"
        );
        assert_eq!(
            state.event_payload_bytes,
            state
                .events
                .iter()
                .map(PendingEvent::payload_bytes)
                .sum::<usize>()
        );
        let bytes = state.event_payload_bytes;
        state.queue(PendingEvent::Terminal(TerminalEvent::Title(
            String::with_capacity(MAX_EVENT_PAYLOAD_BYTES + 1),
        )));
        assert_eq!(state.event_payload_bytes, bytes);
    }
    let (_, more) = terminal.drain_events(&mut |_| None);
    assert!(!more);
    let mut state = terminal.shared.state();
    assert_eq!(state.event_payload_bytes, 0);
    assert!(state.events.is_empty());
    state.queue(PendingEvent::Terminal(TerminalEvent::Title(
        "accepted".into(),
    )));
    assert_eq!(state.events.len(), 1);
    assert_eq!(state.event_payload_bytes, "accepted".len());
}

#[test]
fn dropped_clipboard_chunk_cannot_commit_a_truncated_write() {
    #[derive(Default)]
    struct Host(Vec<crate::TerminalClipboardWriteRequest>);
    impl TerminalReplyHost for Host {
        fn load_clipboard(&mut self, _: TerminalClipboardTarget) -> Option<String> {
            None
        }
        fn write_clipboard(
            &mut self,
            request: crate::TerminalClipboardWriteRequest,
        ) -> crate::TerminalClipboardWriteResult {
            self.0.push(request);
            crate::TerminalClipboardWriteResult::Success {
                remember_permission: false,
            }
        }
    }
    for pressure in ["payload", "count", "compaction"] {
        let terminal = display(6, 2);
        let mut host = Host::default();
        terminal.feed_output(b"\x1b[?5522h\x1b]5522;type=write:id=partial\x1b\\\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;aGVsbG8=\x1b\\");
        terminal.drain_events(&mut host);
        const MISSING_CHUNK: &[u8] = b"\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;d29ybGQ=\x1b\\";
        if pressure == "compaction" {
            terminal.feed_output(MISSING_CHUNK);
        }
        {
            let mut state = terminal.shared.state();
            if pressure == "payload" {
                state.queue(PendingEvent::Terminal(TerminalEvent::Title(
                    String::with_capacity(MAX_EVENT_PAYLOAD_BYTES),
                )));
            } else {
                for _ in state.events.len()..MAX_EVENTS {
                    state.queue(PendingEvent::Terminal(TerminalEvent::Bell));
                }
                if pressure == "compaction" {
                    state.queue(PendingEvent::Terminal(TerminalEvent::Exit));
                }
            }
        }
        if pressure != "compaction" {
            terminal.feed_output(MISSING_CHUNK);
        }
        while terminal.drain_events(&mut host).1 {}
        terminal.feed_output(b"\x1b]5522;type=wdata;\x1b\\");
        terminal.drain_events(&mut host);
        assert!(
            host.0.is_empty(),
            "missing world chunk must abort the hello write under {pressure} pressure"
        );
        assert!(
            terminal
                .shared
                .clipboard
                .lock()
                .unwrap()
                .paste_notification(TerminalClipboardLocation::Clipboard, &["text/plain".into()])
                .is_some(),
            "overflow reset must preserve the host's paste mode"
        );
        terminal.feed_output(b"\x1b]5522;type=write:id=fresh\x1b\\\x1b]5522;type=wdata:mime=dGV4dC9wbGFpbg==;ZnJlc2g=\x1b\\\x1b]5522;type=wdata;\x1b\\");
        terminal.drain_events(&mut host);
        assert_eq!(host.0.len(), 1);
        assert_eq!(host.0[0].contents[0].data, b"fresh");
    }
}

#[test]
fn saturated_event_queue_preserves_exit_and_clipboard_invalidation() {
    for invalidation in [
        KittyClipboardControl::Reset,
        KittyClipboardControl::Set(false),
    ] {
        let mut state = State::new(TerminalSize::default(), &TerminalRuntimeConfig::default());
        state.queue(PendingEvent::KittyControl(invalidation));
        state.queue(PendingEvent::KittyControl(KittyClipboardControl::Set(true)));
        for _ in state.events.len()..MAX_EVENTS {
            state.queue(PendingEvent::Terminal(TerminalEvent::Bell));
        }
        state.queue(PendingEvent::Terminal(TerminalEvent::Exit));
        assert_eq!(state.events.len(), 3);
        assert!(
            matches!(state.pop_event(), Some(PendingEvent::KittyControl(control)) if control == invalidation)
        );
        assert!(matches!(
            state.pop_event(),
            Some(PendingEvent::KittyControl(KittyClipboardControl::Set(true)))
        ));
        // Leave Exit queued and saturate again: a later reset must not
        // discard the process exit that the host has yet to observe.
        for _ in state.events.len()..MAX_EVENTS {
            state.queue(PendingEvent::Terminal(TerminalEvent::Bell));
        }
        state.queue(PendingEvent::KittyControl(KittyClipboardControl::Reset));
        assert!(matches!(
            state.pop_event(),
            Some(PendingEvent::Terminal(TerminalEvent::Exit))
        ));
        assert!(matches!(
            state.pop_event(),
            Some(PendingEvent::KittyControl(KittyClipboardControl::Reset))
        ));
        assert!(state.events.is_empty());
        assert_eq!(state.event_payload_bytes, 0);
    }
}

#[test]
fn display_reply_is_ready_when_the_wakeup_callback_runs() {
    struct Host(Arc<Mutex<Vec<u8>>>);
    impl TerminalReplyHost for Host {
        fn load_clipboard(&mut self, _: TerminalClipboardTarget) -> Option<String> {
            None
        }
        fn protocol_reply(&mut self, bytes: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(bytes);
        }
    }
    let slot = Arc::new(Mutex::new(Weak::<CustomBackend>::new()));
    let replies = Arc::new(Mutex::new(Vec::new()));
    let callback_slot = slot.clone();
    let callback_replies = replies.clone();
    let terminal = Arc::new(CustomBackend::new_display_with_wakeup_notifier(
        TerminalSize::default(),
        None,
        Some(TerminalWakeupNotifier::new(move || {
            let terminal = callback_slot.lock().unwrap().upgrade().unwrap();
            terminal.drain_events(&mut Host(callback_replies.clone()));
        })),
    ));
    *slot.lock().unwrap() = Arc::downgrade(&terminal);
    terminal.feed_output(b"hi\x1b[6n");
    assert_eq!(*replies.lock().unwrap(), b"\x1b[1;3R");
    assert!(!terminal.has_pending_events());
}

#[test]
fn synchronized_output_commits_after_deadline_without_more_input() {
    let (send, receive) = flume::unbounded();
    let terminal = CustomBackend::new_display_with_wakeup_notifier(
        TerminalSize::default(),
        None,
        Some(TerminalWakeupNotifier::new(move || {
            let _ = send.send(());
        })),
    );
    terminal.render_read(true);
    terminal.feed_output(b"\x1b[?2026hdeadline");
    assert_eq!(terminal.snapshot().cells[0].char, ' ');
    receive
        .recv_timeout(Duration::from_secs(2))
        .expect("watchdog commits output");
    assert_eq!(terminal.snapshot().cells[0].char, 'd');
}

#[test]
fn terminal_input_commits_a_pending_synchronized_frame() {
    let terminal = display(20, 2);
    terminal.feed_output(b"\x1b[?2026hvisible-before-input");
    assert_eq!(terminal.snapshot().cells[0].char, ' ');

    terminal.write(b"x");

    let frame: String = terminal
        .snapshot()
        .cells
        .into_iter()
        .map(|cell| cell.char)
        .collect();
    assert!(frame.starts_with("visible-before-input"));
    assert!(terminal.shared.state().engine.modes().synchronized_update);

    terminal.feed_output(b"later\x1b[?2026l");
    let frame: String = terminal
        .snapshot()
        .cells
        .into_iter()
        .map(|cell| cell.char)
        .collect();
    assert!(frame.starts_with("visible-before-inputlater"));
    assert!(!terminal.shared.state().engine.modes().synchronized_update);
}

#[cfg(unix)]
#[test]
fn native_exit_commits_synchronized_tail_before_exit_event() {
    let terminal = CustomBackend::new_with_launch_and_wakeup_notifier(
        TerminalSize::default(),
        None,
        None,
        None,
        None,
        Some(&TerminalLaunch::Program {
            program: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf '\\033[?2026hfinal-tail'".to_owned(),
            ],
        }),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (events, _) = terminal.drain_events(&mut |_| None);
        if events
            .iter()
            .any(|event| matches!(event, TerminalEvent::Exit))
        {
            let text: String = terminal
                .snapshot()
                .cells
                .into_iter()
                .map(|cell| cell.char)
                .collect();
            assert!(text.starts_with("final-tail"));
            break;
        }
        assert!(Instant::now() < deadline, "child exited");
        std::thread::sleep(Duration::from_millis(5));
    }
}
