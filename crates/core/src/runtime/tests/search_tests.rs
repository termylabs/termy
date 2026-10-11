// Search behavior covered through the public runtime API.
use super::*;

#[test]
fn search_includes_scrollback_rows() {
    let terminal = Terminal::new_display(
        TerminalSize {
            cols: 16,
            rows: 2,
            ..test_terminal_size()
        },
        Some(&TerminalRuntimeConfig {
            scrollback_history: 8,
            ..Default::default()
        }),
    );
    terminal.feed_output(b"alpha\r\nbeta\r\ngamma");
    let matches = terminal.search("alpha");
    assert_eq!(terminal.scroll_state().1, 1);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].row, 0);
    assert_eq!(matches[0].start_col, 0);
    assert_eq!(matches[0].end_col, 4);
}

#[test]
fn search_preserves_unicode_clusters_and_physical_columns() {
    let terminal = Terminal::new_display(TerminalSize::default(), None);
    terminal.feed_output("A日本語 e\u{301} 👩🏽‍💻Z".as_bytes());
    for (query, first, last) in [
        ("日本語", 1, 6),
        ("e\u{301}", 8, 8),
        ("\u{301}", 8, 8),
        ("👩🏽‍💻", 10, 11),
        ("👩", 10, 11),
        ("Z", 12, 12),
    ] {
        let matches = terminal.search(query);
        assert_eq!(matches.len(), 1, "query {query}");
        assert_eq!(
            (matches[0].start_col, matches[0].end_col),
            (first, last),
            "query {query}"
        );
    }
    let matches = crate::search::search_frame(&terminal.snapshot(), "日本語");
    assert_eq!(matches.len(), 1);
    assert_eq!((matches[0].start_col, matches[0].end_col), (1, 6));
}

#[test]
fn search_omits_hidden_wide_text_without_shifting_following_columns() {
    let terminal = Terminal::new_display(TerminalSize::default(), None);
    terminal.feed_output("A\x1b[8m界\x1b[0mZ".as_bytes());
    assert!(terminal.search("界").is_empty());
    let matches = terminal.search("Z");
    assert_eq!((matches[0].start_col, matches[0].end_col), (3, 3));
    let matches = crate::search::search_frame(&terminal.snapshot(), "Z");
    assert_eq!((matches[0].start_col, matches[0].end_col), (3, 3));
}

#[test]
fn search_uses_physical_cells_for_separately_rendered_emoji() {
    let terminal = Terminal::new_display(TerminalSize::default(), None);
    terminal.feed_output("👩\x07\u{200d}💻Z".as_bytes());
    for (query, start, end) in [("👩", 0, 1), ("💻", 2, 3), ("Z", 4, 4)] {
        let matches = terminal.search(query);
        assert_eq!(
            (matches[0].start_col, matches[0].end_col),
            (start, end),
            "{query}"
        );
    }
}

#[test]
fn search_clips_wide_clusters_to_one_column_grids() {
    let terminal = Terminal::new_display(
        TerminalSize {
            cols: 1,
            rows: 4,
            ..Default::default()
        },
        None,
    );
    terminal.feed_output("界\r\n👩🏽‍💻".as_bytes());
    for query in ["界", "👩", "💻"] {
        let matches = terminal.search(query);
        assert_eq!(
            (matches[0].start_col, matches[0].end_col),
            (0, 0),
            "{query}"
        );
    }
    let matches = crate::search::search_frame(&terminal.snapshot(), "界");
    assert_eq!((matches[0].start_col, matches[0].end_col), (0, 0));
}
