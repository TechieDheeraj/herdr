use super::*;
use crate::sidebar_color::SIDEBAR_COLOR_PRESETS;

pub(super) fn render_row_color_overlay(
    b: &mut Buffer,
    picker: &super::super::super::row_colors::RowColorPicker,
    p: &Palette,
) -> Option<OverlayRender> {
    let q = popup(b.area, 64, 13)?;
    if q.height < 12 {
        return None;
    }
    let i = panel(b, q, p.accent, p.panel_bg)?;
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        "row color",
        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &picker.label,
        Style::default().fg(p.overlay0),
    );
    let mut swatches = Vec::new();
    let column_width = i.width / 4;
    for (index, (name, color)) in SIDEBAR_COLOR_PRESETS.iter().enumerate() {
        let column = index % 4;
        let x = i.x + column_width * column as u16;
        let width = if column == 3 {
            i.right().saturating_sub(x)
        } else {
            column_width
        };
        let rect = Rect::new(x, i.y + 2 + (index / 4) as u16, width, 1);
        if rect.bottom() > i.bottom() {
            continue;
        }
        let style = Style::default()
            .fg(color.contrast())
            .bg(color.ratatui())
            .add_modifier(if index == picker.selected {
                Modifier::BOLD | Modifier::UNDERLINED
            } else {
                Modifier::empty()
            });
        button(b, rect, name, style);
        swatches.push(rect);
    }
    put_text(
        b,
        i.x,
        i.y + 5,
        i.width,
        "custom hex (#RGB or #RRGGBB)",
        Style::default().fg(p.overlay0),
    );
    let input = Rect::new(i.x, i.y + 6, i.width, 1);
    if input.bottom() <= i.bottom() {
        b.set_style(input, Style::default().fg(p.text).bg(p.surface0));
        put_text(
            b,
            input.x,
            input.y,
            input.width,
            &picker.input,
            Style::default().fg(p.text).bg(p.surface0),
        );
    }
    put_text(
        b,
        i.x,
        i.y + 7,
        i.width,
        picker
            .error
            .as_deref()
            .unwrap_or("click a color, or enter a hex value and press enter"),
        Style::default().fg(if picker.error.is_some() {
            p.red
        } else {
            p.overlay0
        }),
    );
    let rects = row(i, &[9, 16, 12], 2, 9);
    let [apply, clear, cancel] = rects.as_slice() else {
        return None;
    };
    button(
        b,
        *apply,
        " ↵ apply ",
        Style::default()
            .fg(p.accent_foreground())
            .bg(p.accent)
            .add_modifier(Modifier::BOLD),
    );
    let secondary = Style::default().fg(p.text).bg(p.surface0);
    button(b, *clear, " ^c clear color ", secondary);
    button(b, *cancel, " esc cancel ", secondary);
    Some(OverlayRender {
        area: q,
        primary: *apply,
        clear: *clear,
        cancel: *cancel,
        color_swatches: swatches,
        cursor: (input.bottom() <= i.bottom()).then(|| crate::protocol::CursorState {
            x: input
                .x
                .saturating_add(display_width(&picker.input))
                .min(input.right().saturating_sub(1)),
            y: input.y,
            visible: true,
            shape: 0,
        }),
        ..OverlayRender::default()
    })
}
