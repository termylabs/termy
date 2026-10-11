// Rendering and damage conversion between the engine and public terminal API.
use super::*;

pub(super) fn engine_size(size: TerminalSize) -> engine::Size {
    engine::Size {
        cols: usize::from(size.cols),
        rows: usize::from(size.rows),
    }
}
pub(super) fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        cols: size.cols,
        rows: size.rows,
        cell_width: size.cell_width,
        cell_height: size.cell_height,
    }
}
pub(super) fn cursor_shape(style: TerminalCursorStyle) -> engine::CursorShape {
    match style {
        TerminalCursorStyle::Block => engine::CursorShape::Block,
        TerminalCursorStyle::Line => engine::CursorShape::Beam,
    }
}
pub(super) fn rgb(color: engine::Color) -> Option<TerminalColor> {
    color.as_rgb().map(|(r, g, b)| TerminalColor { r, g, b })
}
fn render_color(color: engine::Color, foreground: bool) -> TerminalRenderColor {
    if let Some(rgb) = rgb(color) {
        TerminalRenderColor::Rgb(rgb)
    } else if let Some(index) = color.as_indexed() {
        TerminalRenderColor::Indexed(index)
    } else if foreground {
        TerminalRenderColor::DefaultForeground
    } else {
        TerminalRenderColor::DefaultBackground
    }
}
pub(super) fn render_cell(cell: &engine::Cell, wrapped: bool) -> TerminalRenderCell {
    let style = cell.style;
    let attr = style.attributes;
    TerminalRenderCell {
        text: TerminalRenderText::from_cell_suffix(cell.character, Some(cell.combining())),
        foreground: render_color(style.foreground, true),
        background: render_color(style.background, false),
        underline_color: (style.underline_color != engine::Color::DEFAULT)
            .then(|| render_color(style.underline_color, true)),
        bold: attr & engine::Style::BOLD != 0,
        dim: attr & engine::Style::DIM != 0,
        italic: attr & engine::Style::ITALIC != 0,
        inverse: attr & engine::Style::INVERSE != 0,
        hidden: attr & engine::Style::HIDDEN != 0,
        strikethrough: attr & engine::Style::STRIKE != 0,
        underline_style: match style.underline {
            engine::UnderlineStyle::None => TerminalUnderlineStyle::None,
            engine::UnderlineStyle::Single => TerminalUnderlineStyle::Single,
            engine::UnderlineStyle::Double => TerminalUnderlineStyle::Double,
            engine::UnderlineStyle::Curly => TerminalUnderlineStyle::Curly,
            engine::UnderlineStyle::Dotted => TerminalUnderlineStyle::Dotted,
            engine::UnderlineStyle::Dashed => TerminalUnderlineStyle::Dashed,
        },
        hyperlink: cell.hyperlink().is_some(),
        wide_character_spacer: cell.flags & engine::Cell::WIDE_SPACER != 0,
        leading_wide_character_spacer: cell.flags & engine::Cell::LEADING_WIDE_SPACER != 0,
        line_wrapped: wrapped,
    }
}
fn resolve_color(
    color: TerminalRenderColor,
    palette: &TerminalPalette,
    fallback: TerminalQueryColors,
) -> TermyColor {
    let color = match color {
        TerminalRenderColor::Rgb(rgb) => rgb,
        TerminalRenderColor::Indexed(index) | TerminalRenderColor::DimIndexed(index) => {
            palette.indexed[index as usize].unwrap_or_else(|| fallback.indexed_color(index))
        }
        TerminalRenderColor::DefaultBackground => palette.background.unwrap_or(fallback.background),
        TerminalRenderColor::Cursor => palette
            .cursor
            .or(fallback.cursor)
            .unwrap_or(fallback.foreground),
        _ => palette.foreground.unwrap_or(fallback.foreground),
    };
    TermyColor {
        r: color.r,
        g: color.g,
        b: color.b,
        a: 255,
    }
}
pub(super) fn legacy_cell(
    cell: &engine::Cell,
    wrapped: bool,
    palette: &TerminalPalette,
    fallback: TerminalQueryColors,
) -> TermyCell {
    let attributes = cell.style.attributes;
    let bold = attributes & engine::Style::BOLD != 0;
    let inverse = attributes & engine::Style::INVERSE != 0;
    let foreground = render_color(cell.style.foreground, true);
    let background = render_color(cell.style.background, false);
    let (mut foreground, background) = if inverse {
        (background, foreground)
    } else {
        (foreground, background)
    };
    if bold && let TerminalRenderColor::Indexed(index @ 0..=7) = foreground {
        foreground = TerminalRenderColor::Indexed(index + 8);
    }
    let mut fg = resolve_color(foreground, palette, fallback);
    if attributes & engine::Style::DIM != 0 {
        fg.r /= 2;
        fg.g /= 2;
        fg.b /= 2;
    }
    let spacer = cell.flags & (engine::Cell::WIDE_SPACER | engine::Cell::LEADING_WIDE_SPACER) != 0;
    TermyCell {
        char: cell.character,
        fg,
        bg: resolve_color(background, palette, fallback),
        uses_terminal_default_bg: background == TerminalRenderColor::DefaultBackground,
        bold,
        italic: attributes & engine::Style::ITALIC != 0,
        underline: cell.style.underline != engine::UnderlineStyle::None,
        strikethrough: attributes & engine::Style::STRIKE != 0,
        render_text: !spacer
            && attributes & engine::Style::HIDDEN == 0
            && cell.character != '\0'
            && !cell.character.is_control(),
        wide_character_spacer: spacer,
        line_wrapped: wrapped,
    }
}

pub(super) fn append_search_cell(text: &mut String, cell: &engine::Cell) {
    if cell.flags & engine::Cell::WIDE_SPACER != 0 {
        return;
    }
    if cell.flags & engine::Cell::LEADING_WIDE_SPACER != 0
        || cell.style.attributes & engine::Style::HIDDEN != 0
        || cell.character.is_control()
    {
        text.push(' ');
    } else {
        text.push(cell.character);
        text.push_str(cell.combining());
    }
}
// Legacy frame consumers replay only cell patches, so include the rows moved
// by scroll operations rather than silently dropping that part of the update.
pub(super) fn expand_legacy_scroll_damage(update: &mut TerminalRenderDamageSnapshot, cols: usize) {
    if let TerminalDamageSnapshot::Partial(spans) = &mut update.damage {
        for scroll in &update.scrolls {
            spans.extend((scroll.top..=scroll.bottom).map(|row| TerminalDirtySpan {
                row,
                left_col: 0,
                right_col: cols.saturating_sub(1),
            }));
        }
        normalize_spans(spans);
    }
    update.scrolls.clear();
}

pub(super) fn normalize_spans(spans: &mut Vec<TerminalDirtySpan>) {
    spans.sort_unstable_by_key(|span| (span.row, span.left_col));
    let mut len = 0;
    for index in 0..spans.len() {
        let span = spans[index];
        if len > 0
            && spans[len - 1].row == span.row
            && span.left_col <= spans[len - 1].right_col.saturating_add(1)
        {
            spans[len - 1].right_col = spans[len - 1].right_col.max(span.right_col);
        } else {
            spans[len] = span;
            len += 1;
        }
    }
    spans.truncate(len);
}
