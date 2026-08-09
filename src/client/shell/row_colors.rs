use super::*;
use crate::sidebar_color::{SidebarRowColor, SIDEBAR_COLOR_PRESETS};

pub(super) type EndpointRowColors = HashMap<ClientEndpointId, HashMap<String, SidebarRowColor>>;

#[derive(Debug, Clone)]
pub(super) enum RowColorTarget {
    Workspace(String),
    Tab(String),
}

#[derive(Debug)]
pub(super) struct RowColorPicker {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) target: RowColorTarget,
    pub(super) label: String,
    pub(super) input: String,
    pub(super) replace_on_type: bool,
    pub(super) selected: usize,
    pub(super) error: Option<String>,
}

pub(super) fn lookup(
    colors: &EndpointRowColors,
    endpoint_id: &ClientEndpointId,
    object_id: &str,
) -> Option<SidebarRowColor> {
    colors.get(endpoint_id)?.get(object_id).copied()
}

/// A row override must override dim/conditional token colors too, so every line stays legible.
pub(super) fn apply_contrast(buffer: &mut Buffer, area: Rect, color: SidebarRowColor) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_bg(color.ratatui());
                cell.set_fg(color.contrast());
                cell.set_style(
                    cell.style()
                        .remove_modifier(Modifier::DIM | Modifier::REVERSED),
                );
            }
        }
    }
}

impl ClientShellState {
    pub(super) fn open_row_color_picker(&mut self, target: RowColorTarget) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let (label, color) = match &target {
            RowColorTarget::Workspace(id) => {
                let Some(workspace) = snapshot
                    .workspaces
                    .iter()
                    .find(|workspace| &workspace.workspace_id == id)
                else {
                    return;
                };
                (
                    format!("workspace: {}", workspace.label),
                    lookup(&self.workspace_colors, &self.active_endpoint_id, id),
                )
            }
            RowColorTarget::Tab(id) => {
                let Some(tab) = snapshot.tabs.iter().find(|tab| &tab.tab_id == id) else {
                    return;
                };
                (
                    format!("tab: {}", tab.label),
                    lookup(&self.tab_colors, &self.active_endpoint_id, id),
                )
            }
        };
        let selected = color
            .and_then(|color| {
                SIDEBAR_COLOR_PRESETS
                    .iter()
                    .position(|(_, preset)| *preset == color)
            })
            .unwrap_or(7);
        self.overlay = Some(ClientShellOverlay::RowColor(RowColorPicker {
            endpoint_id: self.active_endpoint_id.clone(),
            target,
            label,
            input: color.unwrap_or(SIDEBAR_COLOR_PRESETS[selected].1).hex(),
            replace_on_type: true,
            selected,
            error: None,
        }));
    }

    pub(super) fn save_row_color(&mut self, clear: bool, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::RowColor(picker)) = self.overlay.as_mut() else {
            return;
        };
        let color = if clear {
            None
        } else {
            let Some(color) = SidebarRowColor::parse_hex(&picker.input) else {
                picker.error = Some("Use #RGB or #RRGGBB".into());
                outcome.repaint = true;
                return;
            };
            Some(color)
        };
        let (map, id) = match &picker.target {
            RowColorTarget::Workspace(id) => (&mut self.workspace_colors, id),
            RowColorTarget::Tab(id) => (&mut self.tab_colors, id),
        };
        if let Some(color) = color {
            map.entry(picker.endpoint_id.clone())
                .or_default()
                .insert(id.clone(), color);
        } else if let Some(colors) = map.get_mut(&picker.endpoint_id) {
            colors.remove(id);
        }
        self.overlay = None;
        outcome.repaint = true;
    }

    pub(super) fn insert_row_color_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::RowColor(picker)) = self.overlay.as_mut() else {
            return false;
        };
        if picker.replace_on_type {
            picker.input.clear();
            picker.replace_on_type = false;
        }
        picker.input.extend(
            text.chars()
                .filter(|character| !character.is_control())
                .take(32usize.saturating_sub(picker.input.chars().count())),
        );
        picker.error = None;
        true
    }

    pub(super) fn row_color_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::KeyModifiers;
        let Some(ClientShellOverlay::RowColor(picker)) = self.overlay.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.save_row_color(false, outcome),
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                self.save_row_color(true, outcome)
            }
            KeyCode::Backspace => {
                if picker.replace_on_type {
                    picker.input.clear();
                } else {
                    picker.input.pop();
                }
                picker.replace_on_type = false;
                picker.error = None;
            }
            KeyCode::Delete => {
                picker.input.clear();
                picker.replace_on_type = false;
                picker.error = None;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                let count = SIDEBAR_COLOR_PRESETS.len();
                let delta = match key.code {
                    KeyCode::Left => count - 1,
                    KeyCode::Up => count - 4,
                    KeyCode::Down => 4,
                    _ => 1,
                };
                picker.selected = (picker.selected + delta) % count;
                picker.input = SIDEBAR_COLOR_PRESETS[picker.selected].1.hex();
                picker.replace_on_type = true;
                picker.error = None;
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.insert_row_color_text(&character.to_string());
            }
            _ => {}
        }
        outcome.repaint = true;
        true
    }

    pub(super) fn row_color_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        if !matches!(self.overlay, Some(ClientShellOverlay::RowColor(_))) {
            return false;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            let point = (mouse.column, mouse.row);
            if contains(self.hits.overlay_primary, point) {
                self.save_row_color(false, outcome);
            } else if contains(self.hits.overlay_clear, point) {
                self.save_row_color(true, outcome);
            } else if contains(self.hits.overlay_cancel, point) {
                self.overlay = None;
            } else if let Some(index) = self
                .hits
                .color_swatches
                .iter()
                .position(|rect| contains(*rect, point))
            {
                if let Some(ClientShellOverlay::RowColor(picker)) = self.overlay.as_mut() {
                    picker.selected = index;
                    picker.input = SIDEBAR_COLOR_PRESETS[index].1.hex();
                }
                self.save_row_color(false, outcome);
            }
            outcome.repaint = true;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_colors_are_scoped_to_machine_and_contrast_all_cells() {
        let remote = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("1234567890abcdef1234567890abcdef")
                .expect("profile"),
        );
        let color = SidebarRowColor::new(255, 180, 220);
        let mut colors = EndpointRowColors::new();
        colors
            .entry(ClientEndpointId::Local)
            .or_default()
            .insert("ws-1".into(), color);
        assert_eq!(lookup(&colors, &remote, "ws-1"), None);
        assert_eq!(
            lookup(&colors, &ClientEndpointId::Local, "ws-1"),
            Some(color)
        );
        let area = Rect::new(0, 0, 4, 2);
        let mut buffer = Buffer::empty(area);
        buffer.set_style(
            area,
            Style::default()
                .fg(ratatui::style::Color::Red)
                .add_modifier(Modifier::DIM),
        );
        apply_contrast(&mut buffer, area, color);
        assert!(buffer.content.iter().all(|cell| cell.fg == color.contrast()
            && cell.bg == color.ratatui()
            && !cell.modifier.contains(Modifier::DIM)));
    }
}
