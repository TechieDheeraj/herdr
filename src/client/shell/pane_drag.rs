//! Client-owned pane-title gestures. Layout changes use the public runtime API.

use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::*;
use crate::api::schema::{
    LayoutDescription, LayoutExportParams, LayoutNode, Method, PaneMoveDestination, PaneMoveParams,
    PanePlacement, PaneSwapParams, PaneTarget, SplitDirection, TabTarget,
};
use crate::layout::{Node, PaneId, TileLayout};

const HOVER_DELAY: Duration = Duration::from_millis(400);
const SCROLL_DELAY: Duration = Duration::from_millis(120);

#[derive(Clone, Debug)]
pub(super) struct PaneDragOrigin {
    endpoint_id: ClientEndpointId,
    boot_id: String,
    generation: Option<u64>,
    pane_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HoverTarget {
    Workspace(String),
    Tab(String),
    WorkspaceScroll(i8),
    TabScroll(i8),
}

#[derive(Clone, Debug)]
struct Hover {
    target: HoverTarget,
    deadline: Instant,
}

#[derive(Clone, Debug)]
struct DropPreview {
    rects: Vec<Rect>,
    label: &'static str,
    valid: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ClientPaneDrag {
    origin: PaneDragOrigin,
    source_pane_id: String,
    source_tab_id: String,
    started_at: Instant,
    press: (u16, u16),
    pointer: (u16, u16),
    dragging: bool,
    hover: Option<Hover>,
    preview: Option<DropPreview>,
    action: Option<Method>,
    layout: Option<(u64, LayoutDescription)>,
    pending_layout: Option<(String, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropZone {
    Left,
    Right,
    Above,
    Below,
    Center,
}

impl DropZone {
    fn placement(self) -> Option<PanePlacement> {
        match self {
            Self::Left => Some(PanePlacement::Left),
            Self::Right => Some(PanePlacement::Right),
            Self::Above => Some(PanePlacement::Above),
            Self::Below => Some(PanePlacement::Below),
            Self::Center => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Left => "move left",
            Self::Right => "move right",
            Self::Above => "move above",
            Self::Below => "move below",
            Self::Center => "swap panes",
        }
    }
}

fn drop_zone(rect: Rect, point: (u16, u16)) -> DropZone {
    let width = rect.width.max(1);
    let height = rect.height.max(1);
    let left = point.0.saturating_sub(rect.x).min(width - 1);
    let right = rect
        .right()
        .saturating_sub(1)
        .saturating_sub(point.0)
        .min(width - 1);
    let top = point.1.saturating_sub(rect.y).min(height - 1);
    let bottom = rect
        .bottom()
        .saturating_sub(1)
        .saturating_sub(point.1)
        .min(height - 1);
    if width >= 8
        && height >= 5
        && left.saturating_mul(4) >= width
        && right.saturating_mul(4) >= width
        && top.saturating_mul(4) >= height
        && bottom.saturating_mul(4) >= height
    {
        return DropZone::Center;
    }
    [
        (u32::from(left) * u32::from(height), DropZone::Left),
        (u32::from(right) * u32::from(height), DropZone::Right),
        (u32::from(top) * u32::from(width), DropZone::Above),
        (u32::from(bottom) * u32::from(width), DropZone::Below),
    ]
    .into_iter()
    .min_by_key(|(distance, _)| *distance)
    .map_or(DropZone::Left, |(_, zone)| zone)
}

fn projected_layout_node(node: &LayoutNode, ids: &mut Vec<String>) -> Option<Node> {
    match node {
        LayoutNode::Pane { pane } => {
            let id = pane.pane_id.as_ref()?;
            if ids.contains(id) {
                return None;
            }
            ids.push(id.clone());
            Some(Node::Pane(PaneId::from_raw(u32::try_from(ids.len()).ok()?)))
        }
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => Some(Node::Split {
            direction: match direction {
                SplitDirection::Right => ratatui::layout::Direction::Horizontal,
                SplitDirection::Down => ratatui::layout::Direction::Vertical,
            },
            ratio: *ratio,
            first: Box::new(projected_layout_node(first, ids)?),
            second: Box::new(projected_layout_node(second, ids)?),
        }),
    }
}

fn geometry_signature(surface: &PaneSurfaceFrame) -> u64 {
    // Geometry joins topology here: a split resize by another client also
    // invalidates the exported ratios used for a placement preview.
    let mut hash = pane_surface_topology_signature(surface);
    for pane in &surface.panes {
        for byte in pane.pane_id.bytes() {
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(byte);
        }
        for value in [pane.rect.x, pane.rect.y, pane.rect.width, pane.rect.height] {
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(value);
        }
    }
    hash
}

fn project_drop(
    description: &LayoutDescription,
    source: &str,
    target: &str,
    placement: PanePlacement,
    area: Rect,
) -> Option<(Rect, Vec<crate::layout::PaneInfo>)> {
    let mut ids = Vec::new();
    let root = projected_layout_node(&description.root, &mut ids)?;
    let target_id =
        PaneId::from_raw(u32::try_from(ids.iter().position(|id| id == target)? + 1).ok()?);
    let source_id = PaneId::from_raw(
        u32::try_from(
            ids.iter()
                .position(|id| id == source)
                .map_or(ids.len() + 1, |index| index + 1),
        )
        .ok()?,
    );
    let layout = TileLayout::from_saved(root, target_id);
    let projected = if ids.iter().any(|id| id == source) {
        layout.projected_relocation(source_id, target_id, placement, 0.5)?
    } else {
        layout.projected_insert(target_id, source_id, placement, 0.5)?
    };
    let panes = projected.panes(area);
    let rect = panes.iter().find(|pane| pane.id == source_id)?.rect;
    Some((rect, panes))
}

impl ClientShellState {
    fn pane_drag_source_is_current(&self, drag: &ClientPaneDrag) -> bool {
        self.active_endpoint_id == drag.origin.endpoint_id
            && self.active_snapshot_generation == drag.origin.generation
            && self.endpoint_is_online(&drag.origin.endpoint_id)
            && self.snapshot.as_deref().is_some_and(|snapshot| {
                snapshot.boot_id == drag.origin.boot_id
                    && snapshot.panes.iter().any(|pane| {
                        pane.pane_id == drag.source_pane_id && pane.tab_id == drag.source_tab_id
                    })
            })
    }

    fn pane_drag_title_at(&self, point: (u16, u16)) -> Option<String> {
        let snapshot = self.snapshot.as_deref()?;
        let surface = self.pane_surface.as_ref()?;
        if self.pending_pane_surface.is_some()
            || self.pane_surface_generation != self.active_snapshot_generation
            || snapshot.boot_id != surface.boot_id
            || snapshot.revision != surface.projection_revision
        {
            return None;
        }
        let (cols, rows) = self.last_composed_size?;
        if !self.layout(cols, rows).mobile_header.is_empty() {
            return None;
        }
        self.hits.panes.iter().find_map(|hit| {
            if hit.popup
                || hit.rect.width <= 4
                || hit.inner_rect.y <= hit.rect.y
                || point.1 != hit.rect.y
            {
                return None;
            }
            let pane = snapshot
                .panes
                .iter()
                .find(|pane| pane.pane_id == hit.pane_id)?;
            let label = pane.label.as_deref()?.trim();
            if label.is_empty() {
                return None;
            }
            let title_width =
                UnicodeWidthStr::width(label).min(usize::from(hit.rect.width - 4)) + 2;
            contains(
                Rect::new(hit.rect.x + 1, hit.rect.y, title_width as u16, 1),
                point,
            )
            .then(|| hit.pane_id.clone())
        })
    }

    pub(super) fn restore_pane_drag_origin(
        &mut self,
        origin: &PaneDragOrigin,
        outcome: &mut ClientShellInput,
    ) {
        if self.active_endpoint_id == origin.endpoint_id
            && self.active_snapshot_generation == origin.generation
            && self.snapshot.as_deref().is_some_and(|snapshot| {
                snapshot.boot_id == origin.boot_id
                    && snapshot
                        .panes
                        .iter()
                        .any(|pane| pane.pane_id == origin.pane_id)
            })
        {
            self.push_endpoint_method(
                Method::PaneFocus(PaneTarget {
                    pane_id: origin.pane_id.clone(),
                }),
                outcome,
            );
        }
    }

    pub(super) fn cancel_pane_drag(&mut self, outcome: &mut ClientShellInput) {
        if let Some(drag) = self.pane_drag.take() {
            self.restore_pane_drag_origin(&drag.origin, outcome);
            outcome.repaint = true;
        }
    }

    pub(super) fn handle_pane_drag_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let point = (mouse.column, mouse.row);
        if self.pane_drag.is_some() {
            let Some(mut drag) = self.pane_drag.take() else {
                return false;
            };
            if !self.pane_drag_source_is_current(&drag)
                || self.overlay.is_some()
                || self.popup_terminal_id.is_some()
            {
                outcome.repaint = true;
                return matches!(mouse.kind, MouseEventKind::Up(_) | MouseEventKind::Drag(_));
            }
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                    drag.pointer = point;
                    drag.dragging |= drag
                        .press
                        .0
                        .abs_diff(point.0)
                        .max(drag.press.1.abs_diff(point.1))
                        >= 1;
                    if drag.dragging {
                        self.update_pane_drag_target(&mut drag, outcome);
                    }
                    self.pane_drag = Some(drag);
                    outcome.repaint = true;
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    outcome.repaint = true;
                    if !drag.dragging {
                        return true;
                    }
                    drag.pointer = point;
                    self.update_pane_drag_target(&mut drag, outcome);
                    if let Some(method) = drag.action {
                        if !self.push_endpoint_method_with_kind(
                            method,
                            PendingEndpointKind::PaneDragDrop {
                                origin: drag.origin.clone(),
                            },
                            outcome,
                        ) {
                            self.restore_pane_drag_origin(&drag.origin, outcome);
                        }
                    } else {
                        self.restore_pane_drag_origin(&drag.origin, outcome);
                    }
                    return true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.pane_drag = Some(drag);
                    return true;
                }
                MouseEventKind::Down(_) => {
                    self.restore_pane_drag_origin(&drag.origin, outcome);
                    outcome.repaint = true;
                    return true;
                }
                _ => {
                    self.pane_drag = Some(drag);
                    return true;
                }
            }
        }
        if !self.config.mouse_capture
            || self.overlay.is_some()
            || self.popup_terminal_id.is_some()
            || self.chrome_drag.is_some()
            || self.pane_mouse_gesture.is_some()
            || mouse.modifiers != KeyModifiers::empty()
            || mouse.kind != MouseEventKind::Down(MouseButton::Left)
        {
            return false;
        }
        let Some(pane_id) = self.pane_drag_title_at(point) else {
            return false;
        };
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        let Some(tab_id) = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .map(|pane| pane.tab_id.clone())
        else {
            return false;
        };
        let origin = PaneDragOrigin {
            endpoint_id: self.active_endpoint_id.clone(),
            boot_id: snapshot.boot_id.clone(),
            generation: self.active_snapshot_generation,
            pane_id: snapshot
                .focused_pane_id
                .clone()
                .unwrap_or_else(|| pane_id.clone()),
        };
        self.selection = None;
        self.word_selection_gesture = None;
        self.selection_repaint_deadline = None;
        self.clear_link_hover();
        self.last_pane_click = None;
        self.selection_highlight_clear_deadline = None;
        self.stop_selection_autoscroll();
        self.workspace_press = None;
        self.tab_press = None;
        self.last_workspace_blank_click = None;
        self.push_endpoint_method(
            Method::PaneFocus(PaneTarget {
                pane_id: pane_id.clone(),
            }),
            outcome,
        );
        self.pane_drag = Some(ClientPaneDrag {
            origin,
            source_pane_id: pane_id,
            source_tab_id: tab_id,
            started_at: Instant::now(),
            press: point,
            pointer: point,
            dragging: false,
            hover: None,
            preview: None,
            action: None,
            layout: None,
            pending_layout: None,
        });
        outcome.repaint = true;
        true
    }

    fn set_pane_drag_hover(drag: &mut ClientPaneDrag, target: HoverTarget, delay: Duration) {
        if drag
            .hover
            .as_ref()
            .is_some_and(|hover| hover.target == target)
        {
            return;
        }
        drag.hover = Some(Hover {
            target,
            deadline: Instant::now() + delay,
        });
    }

    fn update_pane_drag_target(
        &mut self,
        drag: &mut ClientPaneDrag,
        outcome: &mut ClientShellInput,
    ) {
        drag.action = None;
        drag.preview = None;
        let point = drag.pointer;
        let coherent = self
            .snapshot
            .as_deref()
            .zip(self.pane_surface.as_ref())
            .is_some_and(|(snapshot, surface)| {
                self.pending_pane_surface.is_none()
                    && self.pane_surface_generation == self.active_snapshot_generation
                    && snapshot.boot_id == surface.boot_id
                    && snapshot.revision == surface.projection_revision
            });
        if coherent {
            if let Some(hit) = self
                .hits
                .panes
                .iter()
                .find(|hit| contains(hit.rect, point))
                .cloned()
            {
                drag.hover = None;
                self.update_pane_drop_preview(drag, &hit, outcome);
                return;
            }
        }
        if contains(self.hits.tab_scroll_left, point) && self.tab_scroll > 0 {
            Self::set_pane_drag_hover(drag, HoverTarget::TabScroll(-1), SCROLL_DELAY);
            return;
        }
        if contains(self.hits.tab_scroll_right, point) {
            Self::set_pane_drag_hover(drag, HoverTarget::TabScroll(1), SCROLL_DELAY);
            return;
        }
        if contains(self.hits.workspace_body, point) {
            let first_y = self
                .hits
                .workspaces
                .first()
                .map(|hit| hit.rect.y)
                .unwrap_or(self.hits.workspace_body.y);
            let last_y = self
                .hits
                .workspaces
                .last()
                .map(|hit| hit.rect.bottom().saturating_sub(1))
                .unwrap_or(self.hits.workspace_body.bottom());
            if point.1 <= first_y && self.workspace_scroll > 0 {
                Self::set_pane_drag_hover(drag, HoverTarget::WorkspaceScroll(-1), SCROLL_DELAY);
                return;
            }
            if point.1 >= last_y && self.workspace_scroll < self.hits.workspace_max_scroll {
                Self::set_pane_drag_hover(drag, HoverTarget::WorkspaceScroll(1), SCROLL_DELAY);
                return;
            }
            let target = self
                .hits
                .workspaces
                .iter()
                .find(|hit| contains(hit.rect, point))
                .or_else(|| {
                    self.hits.workspaces.last().filter(|hit| {
                        self.workspace_scroll == self.hits.workspace_max_scroll
                            && point.1 >= hit.rect.bottom()
                            && point.0 >= hit.rect.x
                            && point.0 < hit.rect.right()
                    })
                });
            if let Some(hit) = target {
                if hit.endpoint_id != drag.origin.endpoint_id {
                    drag.hover = None;
                    drag.preview = Some(DropPreview {
                        rects: vec![hit.rect],
                        label: "same machine session only",
                        valid: false,
                    });
                    return;
                }
                if self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.focused_workspace_id.as_deref())
                    != Some(hit.workspace_id.as_str())
                {
                    Self::set_pane_drag_hover(
                        drag,
                        HoverTarget::Workspace(hit.workspace_id.clone()),
                        HOVER_DELAY,
                    );
                    return;
                }
            }
        }
        if let Some((_, tab_id)) = self
            .hits
            .tabs
            .iter()
            .find(|(rect, _)| contains(*rect, point))
        {
            if self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_tab_id.as_deref())
                != Some(tab_id.as_str())
            {
                Self::set_pane_drag_hover(drag, HoverTarget::Tab(tab_id.clone()), HOVER_DELAY);
                return;
            }
        }
        drag.hover = None;
    }

    fn update_pane_drop_preview(
        &mut self,
        drag: &mut ClientPaneDrag,
        hit: &PaneHit,
        outcome: &mut ClientShellInput,
    ) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(tab) = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == hit.pane_id)
            .and_then(|pane| snapshot.tabs.iter().find(|tab| tab.tab_id == pane.tab_id))
        else {
            return;
        };
        let tab_id = tab.tab_id.clone();
        let same_tab = tab_id == drag.source_tab_id;
        let zone = drop_zone(hit.rect, drag.pointer);
        let invalid = if tab.zoomed {
            Some("unzoom to move panes")
        } else if hit.pane_id == drag.source_pane_id {
            Some("choose another pane")
        } else if zone == DropZone::Center && !same_tab {
            Some("move toward an edge")
        } else {
            None
        };
        if let Some(label) = invalid {
            drag.preview = Some(DropPreview {
                rects: vec![hit.rect],
                label,
                valid: false,
            });
            return;
        }
        if zone == DropZone::Center {
            let mut rects = vec![hit.rect];
            if let Some(source) = self
                .hits
                .panes
                .iter()
                .find(|pane| pane.pane_id == drag.source_pane_id)
            {
                rects.push(source.rect);
            }
            drag.preview = Some(DropPreview {
                rects,
                label: zone.label(),
                valid: true,
            });
            drag.action = Some(Method::PaneSwap(PaneSwapParams {
                source_pane_id: Some(drag.source_pane_id.clone()),
                target_pane_id: Some(hit.pane_id.clone()),
                ..PaneSwapParams::default()
            }));
            return;
        }
        let Some(surface) = self.pane_surface.as_ref() else {
            return;
        };
        let signature = geometry_signature(surface);
        if !drag
            .layout
            .as_ref()
            .is_some_and(|(saved, layout)| *saved == signature && layout.tab_id == tab_id)
        {
            if drag.pending_layout.as_ref() != Some(&(tab_id.clone(), signature)) {
                drag.pending_layout = Some((tab_id.clone(), signature));
                self.push_endpoint_method_with_kind(
                    Method::LayoutExport(LayoutExportParams {
                        tab_id: Some(tab_id.clone()),
                        pane_id: None,
                    }),
                    PendingEndpointKind::PaneDragLayout {
                        started_at: drag.started_at,
                        tab_id,
                        signature,
                    },
                    outcome,
                );
            }
            drag.preview = Some(DropPreview {
                rects: vec![hit.rect],
                label: "loading layout",
                valid: false,
            });
            return;
        }
        let Some(placement) = zone.placement() else {
            return;
        };
        let Some((_, layout)) = drag.layout.as_ref() else {
            return;
        };
        let Some((cols, rows)) = self.last_composed_size else {
            return;
        };
        let area = self.layout(cols, rows).pane_surface;
        let Some((rect, panes)) =
            project_drop(layout, &drag.source_pane_id, &hit.pane_id, placement, area)
        else {
            return;
        };
        // Each pane needs room for a border, title and at least one content cell.
        if panes
            .iter()
            .any(|pane| pane.rect.width < 5 || pane.rect.height < 3)
        {
            drag.preview = Some(DropPreview {
                rects: vec![hit.rect],
                label: "not enough room",
                valid: false,
            });
            return;
        }
        drag.preview = Some(DropPreview {
            rects: vec![rect],
            label: zone.label(),
            valid: true,
        });
        drag.action = Some(Method::PaneMove(PaneMoveParams {
            pane_id: drag.source_pane_id.clone(),
            destination: PaneMoveDestination::TabPlacement {
                tab_id,
                target_pane_id: hit.pane_id.clone(),
                placement,
                moved_ratio: None,
            },
            focus: true,
        }));
    }

    pub(super) fn complete_pane_drag_layout(
        &mut self,
        started_at: Instant,
        tab_id: String,
        signature: u64,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(mut drag) = self.pane_drag.take() else {
            return;
        };
        if drag.started_at == started_at
            && drag.pending_layout.as_ref() == Some(&(tab_id.clone(), signature))
        {
            if let Ok(crate::api::schema::ResponseResult::LayoutExport { layout }) = result {
                if layout.tab_id == tab_id && !layout.zoomed {
                    drag.pending_layout = None;
                    drag.layout = Some((signature, layout));
                }
            }
            if self.pane_drag_source_is_current(&drag) {
                self.update_pane_drag_target(&mut drag, outcome);
            }
            outcome.repaint = true;
        }
        self.pane_drag = Some(drag);
    }

    pub(crate) fn tick_pane_drag(&mut self, now: Instant) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let Some(mut drag) = self.pane_drag.take() else {
            return outcome;
        };
        if !self.pane_drag_source_is_current(&drag)
            || self.overlay.is_some()
            || self.popup_terminal_id.is_some()
        {
            outcome.repaint = true;
            return outcome;
        }
        if !drag.dragging {
            self.pane_drag = Some(drag);
            return outcome;
        }
        self.update_pane_drag_target(&mut drag, &mut outcome);
        let target = drag
            .hover
            .as_ref()
            .filter(|hover| hover.deadline <= now)
            .map(|hover| hover.target.clone());
        if let Some(target) = target {
            match target {
                HoverTarget::Workspace(workspace_id) => {
                    let first_tab = self
                        .snapshot
                        .as_deref()
                        .and_then(|snapshot| {
                            snapshot
                                .tabs
                                .iter()
                                .filter(|tab| tab.workspace_id == workspace_id)
                                .min_by_key(|tab| tab.number)
                        })
                        .map(|tab| tab.tab_id.clone());
                    if let Some(tab_id) = first_tab {
                        self.push_endpoint_method(
                            Method::TabFocus(TabTarget { tab_id }),
                            &mut outcome,
                        );
                    }
                    drag.hover = None;
                    drag.layout = None;
                }
                HoverTarget::Tab(tab_id) => {
                    self.push_endpoint_method(Method::TabFocus(TabTarget { tab_id }), &mut outcome);
                    drag.hover = None;
                    drag.layout = None;
                }
                HoverTarget::WorkspaceScroll(direction) => {
                    self.workspace_scroll = if direction < 0 {
                        self.workspace_scroll.saturating_sub(1)
                    } else {
                        self.workspace_scroll
                            .saturating_add(1)
                            .min(self.hits.workspace_max_scroll)
                    };
                    self.reveal_focused_workspace = false;
                    self.reveal_navigation_workspace = false;
                    if let Some(hover) = drag.hover.as_mut() {
                        hover.deadline = now + SCROLL_DELAY;
                    }
                }
                HoverTarget::TabScroll(direction) => {
                    let max = self
                        .snapshot
                        .as_deref()
                        .map(|snapshot| {
                            snapshot
                                .tabs
                                .iter()
                                .filter(|tab| {
                                    Some(tab.workspace_id.as_str())
                                        == snapshot.focused_workspace_id.as_deref()
                                })
                                .count()
                                .saturating_sub(1)
                        })
                        .unwrap_or(0);
                    self.tab_scroll = if direction < 0 {
                        self.tab_scroll.saturating_sub(1)
                    } else {
                        self.tab_scroll.saturating_add(1).min(max)
                    };
                    self.reveal_focused_tab = false;
                    if let Some(hover) = drag.hover.as_mut() {
                        hover.deadline = now + SCROLL_DELAY;
                    }
                }
            }
            drag.action = None;
            drag.preview = None;
            outcome.repaint = true;
        }
        self.pane_drag = Some(drag);
        outcome
    }

    pub(super) fn pane_drag_deadline(&self) -> Option<Instant> {
        self.pane_drag
            .as_ref()?
            .hover
            .as_ref()
            .map(|hover| hover.deadline)
    }

    pub(super) fn compose_pane_drag_preview(
        &self,
        frame: &mut FrameData,
        occlusion: &mut crate::kitty_graphics::surface::Occlusion,
    ) {
        if self.overlay.is_some() || self.popup_terminal_id.is_some() {
            return;
        }
        let Some(drag) = self
            .pane_drag
            .as_ref()
            .filter(|drag| self.pane_drag_source_is_current(drag))
        else {
            return;
        };
        let Some(preview) = drag.preview.as_ref() else {
            return;
        };
        let Some(mut buffer) = frame.to_ratatui_buffer() else {
            return;
        };
        let color = if preview.valid {
            self.config.palette.accent
        } else {
            self.config.palette.overlay0
        };
        for rect in &preview.rects {
            let rect = rect.intersection(buffer.area);
            if rect.width < 2 || rect.height < 2 {
                continue;
            }
            // Only the border and title cover terminal graphics. Images inside
            // the proposed pane and in unrelated panes remain visible.
            occlusion.cover(Rect::new(rect.x, rect.y, rect.width, 1));
            occlusion.cover(Rect::new(rect.x, rect.bottom() - 1, rect.width, 1));
            occlusion.cover(Rect::new(rect.x, rect.y, 1, rect.height));
            occlusion.cover(Rect::new(rect.right() - 1, rect.y, 1, rect.height));
            let block = ratatui::widgets::Block::bordered()
                .border_type(ratatui::widgets::BorderType::Thick)
                .border_style(Style::default().fg(color))
                .title(preview.label);
            ratatui::widgets::Widget::render(block, rect, &mut buffer);
        }
        frame.replace_from_ratatui_buffer_preserving_effects(&buffer, None);
    }

    pub(super) fn handle_workspace_blank_click(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let point = (mouse.column, mouse.row);
        let blank = self.config.mouse_capture
            && contains(self.hits.workspace_body, point)
            && !contains(self.hits.workspace_scrollbar, point)
            && !self
                .hits
                .workspaces
                .iter()
                .any(|hit| contains(hit.rect, point))
            && !self
                .hits
                .machines
                .iter()
                .any(|hit| contains(hit.rect, point));
        if mouse.kind != MouseEventKind::Down(MouseButton::Left)
            || !blank
            || !mouse.modifiers.is_empty()
        {
            if matches!(
                mouse.kind,
                MouseEventKind::Down(_)
                    | MouseEventKind::Drag(_)
                    | MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
            ) {
                self.last_workspace_blank_click = None;
            }
            return false;
        }
        outcome.repaint |= self.selection.take().is_some();
        self.stop_selection_autoscroll();
        self.selection_highlight_clear_deadline = None;
        self.word_selection_gesture = None;
        self.selection_repaint_deadline = None;
        outcome.repaint |= self.clear_link_hover();
        self.last_pane_click = None;
        self.workspace_press = None;
        self.tab_press = None;
        let now = Instant::now();
        let double =
            self.last_workspace_blank_click
                .take()
                .is_some_and(|(endpoint, previous, at)| {
                    endpoint == self.active_endpoint_id
                        && now.saturating_duration_since(at) <= Duration::from_millis(350)
                        && previous.0.abs_diff(point.0) <= 1
                        && previous.1.abs_diff(point.1) <= 1
                });
        if double {
            self.record_binding(
                crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewWorkspace),
                outcome,
            );
        } else {
            self.last_workspace_blank_click = Some((self.active_endpoint_id.clone(), point, now));
        }
        true
    }
}

#[cfg(test)]
#[path = "pane_drag_tests.rs"]
mod tests;
