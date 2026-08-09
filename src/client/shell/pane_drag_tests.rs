use super::*;
use crate::protocol::{PaneSurfacePane, PaneSurfaceSplit, PaneSurfaceSplitDirection};

fn pane_node(id: &str) -> LayoutNode {
    LayoutNode::Pane {
        pane: crate::api::schema::LayoutPane {
            pane_id: Some(id.into()),
            ..Default::default()
        },
    }
}

fn pair_layout() -> LayoutDescription {
    LayoutDescription {
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        zoomed: false,
        focused_pane_id: "pane_1".into(),
        root: LayoutNode::Split {
            direction: SplitDirection::Right,
            ratio: 0.5,
            first: Box::new(pane_node("pane_1")),
            second: Box::new(pane_node("pane_2")),
        },
    }
}

fn show(state: &mut ClientShellState, description: &LayoutDescription) {
    let outer = state.layout(120, 40).pane_surface;
    let area = Rect::new(0, 0, outer.width, outer.height);
    let mut ids = Vec::new();
    let tree = projected_layout_node(&description.root, &mut ids).expect("layout");
    let layout = TileLayout::from_saved(tree, PaneId::from_raw(1));
    let snapshot = state.snapshot.as_deref().expect("snapshot");
    let surface = PaneSurfaceFrame {
        boot_id: snapshot.boot_id.clone(),
        projection_revision: snapshot.revision,
        surface_revision: 1,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&Buffer::empty(area), None, &[]),
        panes: layout
            .panes(area)
            .into_iter()
            .map(|pane| PaneSurfacePane {
                pane_id: ids[pane.id.raw() as usize - 1].clone(),
                content_revision: 0,
                rect: pane.rect.into(),
                inner_rect: Rect::new(
                    pane.rect.x + 1,
                    pane.rect.y + 1,
                    pane.rect.width - 2,
                    pane.rect.height - 2,
                )
                .into(),
                scrollbar_rect: None,
                scroll: None,
                focused: pane.is_focused,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            })
            .collect(),
        splits: layout
            .splits(area)
            .into_iter()
            .map(|split| PaneSurfaceSplit {
                direction: match split.direction {
                    ratatui::layout::Direction::Horizontal => PaneSurfaceSplitDirection::Horizontal,
                    ratatui::layout::Direction::Vertical => PaneSurfaceSplitDirection::Vertical,
                },
                pos: split.pos,
                area: split.area.into(),
                hit_rect: Rect::new(split.pos, 0, 1, area.height).into(),
                path: split.path,
            })
            .collect(),
        popup: None,
        graphics: Default::default(),
    };
    state.set_pane_surface(surface);
    state.compose(120, 40).expect("composed");
}

fn pair_state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut snapshot = super::super::tests::snapshot();
    snapshot.panes[0].label = Some("pane 1".into());
    let mut second = snapshot.panes[0].clone();
    second.pane_id = "pane_2".into();
    second.label = Some("pane 2".into());
    second.focused = false;
    snapshot.panes.push(second);
    state.set_snapshot(Box::new(snapshot));
    show(&mut state, &pair_layout());
    state
}

fn mouse(
    state: &mut ClientShellState,
    kind: MouseEventKind,
    point: (u16, u16),
) -> ClientShellInput {
    state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
        kind,
        column: point.0,
        row: point.1,
        modifiers: KeyModifiers::empty(),
    })])
}

fn press_second(state: &mut ClientShellState) {
    let hit = state.hits.panes[1].clone();
    let outcome = mouse(
        state,
        MouseEventKind::Down(MouseButton::Left),
        (hit.rect.x + 2, hit.rect.y),
    );
    assert!(outcome.requests.is_empty());
    assert!(state.selection.is_none());
    assert!(state.pane_drag.is_some());
}

fn complete_layout(
    state: &mut ClientShellState,
    outcome: &ClientShellInput,
    layout: LayoutDescription,
) {
    let request_id = outcome
        .actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, Method::LayoutExport(_)) =>
            {
                Some(request.id.clone())
            }
            _ => None,
        })
        .expect("layout export request");
    state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::LayoutExport { layout }),
    );
}

#[test]
fn title_drag_swaps_center_without_terminal_selection() {
    let mut state = pair_state();
    press_second(&mut state);
    let target = state.hits.panes[0].rect;
    let point = (target.x + target.width / 2, target.y + target.height / 2);
    mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
    let preview = state
        .pane_drag
        .as_ref()
        .and_then(|drag| drag.preview.as_ref())
        .expect("preview");
    assert!(preview.valid);
    assert_eq!(preview.rects.len(), 2);
    let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point);
    assert!(released.actions.iter().any(|action| matches!(action,
        ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::PaneSwap(params)
            if params.source_pane_id.as_deref() == Some("pane_2") && params.target_pane_id.as_deref() == Some("pane_1")))));
    assert!(state.selection.is_none());
    assert!(state.pane_drag.is_none());
}

#[test]
fn edge_drag_reorients_with_exact_layout_preview() {
    let mut state = pair_state();
    press_second(&mut state);
    let target = state.hits.panes[0].rect;
    let point = (target.x + target.width / 2, target.bottom() - 1);
    let pending = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
    assert!(
        !state
            .pane_drag
            .as_ref()
            .unwrap()
            .preview
            .as_ref()
            .unwrap()
            .valid
    );
    complete_layout(&mut state, &pending, pair_layout());
    let preview = state.pane_drag.as_ref().unwrap().preview.as_ref().unwrap();
    assert!(preview.valid);
    let area = state.layout(120, 40).pane_surface;
    assert_eq!(
        preview.rects[0],
        Rect::new(
            area.x,
            area.y + (f32::from(area.height) * 0.5).round() as u16,
            area.width,
            area.height / 2
        )
    );
    let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point);
    assert!(released.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(&request.method, Method::PaneMove(params) if params.pane_id == "pane_2"
            && matches!(&params.destination, PaneMoveDestination::TabPlacement { placement: PanePlacement::Below, .. })))));
}

#[test]
fn canceled_drag_restores_original_pane_and_ignores_late_layout_reply() {
    let mut state = pair_state();
    press_second(&mut state);
    let target = state.hits.panes[0].rect;
    let point = (target.x + target.width / 2, target.bottom() - 1);
    let pending = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
    let outcome = state.handle_input_bytes(b"\x1b");
    // Escape can be retained by the input decoder; the route itself is deterministic.
    let mut outcome = if state.pane_drag.is_some() {
        let mut outcome = outcome;
        state.handle_key(
            crate::input::TerminalKey::from(crossterm::event::KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::empty(),
            )),
            &mut outcome,
        );
        outcome
    } else {
        outcome
    };
    assert!(state.pane_drag.is_none());
    assert!(outcome.actions.drain(..).any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(&request.method, Method::PaneFocus(params) if params.pane_id == "pane_1"))
    ));
    complete_layout(&mut state, &pending, pair_layout());
    assert!(state.pane_drag.is_none());
}

#[test]
fn stale_geometry_forces_new_layout_export_before_drop() {
    let mut state = pair_state();
    press_second(&mut state);
    let target = state.hits.panes[0].rect;
    let point = (target.x + 2, target.bottom() - 1);
    let pending = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
    complete_layout(&mut state, &pending, pair_layout());
    let mut resized = pair_layout();
    if let LayoutNode::Split { ratio, .. } = &mut resized.root {
        *ratio = 0.7;
    }
    show(&mut state, &resized);
    let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point);
    assert!(!released.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::PaneMove(_) | Method::PaneSwap(_)))
    ));
}

#[test]
fn swapping_pane_ids_invalidates_cached_geometry_even_when_splits_do_not_change() {
    let state = pair_state();
    let surface = state.pane_surface.as_ref().expect("surface");
    let mut swapped = surface.clone();
    swapped.panes[0].pane_id = "pane_2".into();
    swapped.panes[1].pane_id = "pane_1".into();
    assert_eq!(
        pane_surface_topology_signature(surface),
        pane_surface_topology_signature(&swapped)
    );
    assert_ne!(geometry_signature(surface), geometry_signature(&swapped));
}

#[test]
fn workspace_autoscroll_reaches_last_workspace_in_both_directions() {
    let mut state = pair_state();
    let mut snapshot = state.snapshot.as_deref().unwrap().clone();
    let workspace = snapshot.workspaces[0].clone();
    let tab = snapshot.tabs[0].clone();
    for number in 2..=18 {
        let mut workspace = workspace.clone();
        workspace.workspace_id = format!("ws_{number}");
        workspace.active_tab_id = format!("tab_{number}");
        workspace.number = number;
        workspace.focused = false;
        snapshot.workspaces.push(workspace);
        let mut tab = tab.clone();
        tab.workspace_id = format!("ws_{number}");
        tab.tab_id = format!("tab_{number}");
        tab.focused = false;
        snapshot.tabs.push(tab);
    }
    state.set_snapshot(Box::new(snapshot));
    show(&mut state, &pair_layout());
    press_second(&mut state);
    let body = state.hits.workspace_body;
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        (body.x + 3, body.bottom() - 1),
    );
    let mut selected_last = false;
    for step in 1..100 {
        let tick = state.tick_pane_drag(Instant::now() + Duration::from_secs(step));
        selected_last |= tick.actions.iter().any(|action| {
            matches!(action, ClientShellAction::Endpoint { request, .. }
            if matches!(&request.method, Method::TabFocus(params) if params.tab_id == "tab_18"))
        });
        state.compose(120, 40).expect("scroll composition");
        if selected_last {
            break;
        }
    }
    assert!(
        selected_last,
        "bottom hover reaches the last workspace including the blank trailing row"
    );
    assert_eq!(state.workspace_scroll, state.hits.workspace_max_scroll);
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        (body.x + 3, body.y),
    );
    for step in 1..100 {
        state.tick_pane_drag(Instant::now() + Duration::from_secs(step));
        state.compose(120, 40).expect("reverse scroll composition");
        if state.workspace_scroll == 0 {
            break;
        }
    }
    assert_eq!(state.workspace_scroll, 0);
}

#[test]
fn workspace_blank_double_click_creates_only_on_second_click() {
    let mut state = pair_state();
    let body = state.hits.workspace_body;
    let point = (body.x + 2, body.bottom() - 1);
    let first = mouse(&mut state, MouseEventKind::Down(MouseButton::Left), point);
    assert!(first.actions.is_empty());
    mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point);
    let second = mouse(&mut state, MouseEventKind::Down(MouseButton::Left), point);
    assert!(second.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::WorkspaceCreate(_)))
    ));
}

#[test]
fn cross_machine_hover_never_switches_or_moves_the_source() {
    let mut state = pair_state();
    press_second(&mut state);
    let hit = &mut state.hits.workspaces[0];
    hit.endpoint_id = ClientEndpointId::Ssh(crate::client::endpoint::ProfileId::generate());
    let point = (hit.rect.x + 2, hit.rect.y);
    mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
    let tick = state.tick_pane_drag(Instant::now() + Duration::from_secs(2));
    assert!(tick.actions.is_empty());
    let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point);
    assert!(!released.actions.iter().any(|action| match action {
        ClientShellAction::ActivateEndpoint { .. } => true,
        ClientShellAction::Endpoint { request, .. } =>
            matches!(request.method, Method::PaneMove(_) | Method::PaneSwap(_)),
        _ => false,
    }));
}

#[test]
fn single_pane_title_is_a_drag_handle_and_content_remains_selectable() {
    let mut state = pair_state();
    let mut snapshot = state.snapshot.as_deref().unwrap().clone();
    snapshot.panes.truncate(1);
    state.set_snapshot(Box::new(snapshot));
    let mut layout = pair_layout();
    layout.root = pane_node("pane_1");
    show(&mut state, &layout);
    let pane = state.hits.panes[0].clone();
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        (pane.rect.x + 2, pane.rect.y),
    );
    assert!(state.pane_drag.is_some());
    assert!(state.selection.is_none());
    mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        (pane.rect.x + 2, pane.rect.y),
    );
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        (pane.inner_rect.x + 2, pane.inner_rect.y + 1),
    );
    assert!(state.pane_drag.is_none());
    assert!(state.selection.is_some());
}

#[test]
fn hover_to_another_tab_then_move_uses_original_pane_identity() {
    let mut state = pair_state();
    let mut snapshot = state.snapshot.as_deref().unwrap().clone();
    let mut tab = snapshot.tabs[0].clone();
    tab.tab_id = "tab_2".into();
    tab.number = 2;
    tab.focused = false;
    snapshot.tabs.push(tab);
    let mut pane = snapshot.panes[0].clone();
    pane.pane_id = "pane_3".into();
    pane.tab_id = "tab_2".into();
    pane.focused = false;
    snapshot.panes.push(pane);
    state.set_snapshot(Box::new(snapshot.clone()));
    show(&mut state, &pair_layout());
    press_second(&mut state);
    let tab_rect = state
        .hits
        .tabs
        .iter()
        .find(|(_, id)| id == "tab_2")
        .unwrap()
        .0;
    mouse(
        &mut state,
        MouseEventKind::Drag(MouseButton::Left),
        (tab_rect.x + 1, tab_rect.y),
    );
    let tick = state.tick_pane_drag(Instant::now() + Duration::from_secs(1));
    assert!(tick.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(&request.method, Method::TabFocus(params) if params.tab_id == "tab_2"))
    ));
    snapshot.revision += 1;
    snapshot.focused_tab_id = Some("tab_2".into());
    snapshot.focused_pane_id = Some("pane_3".into());
    snapshot.workspaces[0].active_tab_id = "tab_2".into();
    for tab in &mut snapshot.tabs {
        tab.focused = tab.tab_id == "tab_2";
    }
    for pane in &mut snapshot.panes {
        pane.focused = pane.pane_id == "pane_3";
    }
    state.set_snapshot(Box::new(snapshot));
    let mut destination = pair_layout();
    destination.tab_id = "tab_2".into();
    destination.focused_pane_id = "pane_3".into();
    destination.root = pane_node("pane_3");
    show(&mut state, &destination);
    let target = state.hits.panes[0].rect;
    let center = (target.x + target.width / 2, target.y + target.height / 2);
    mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), center);
    assert!(
        !state
            .pane_drag
            .as_ref()
            .unwrap()
            .preview
            .as_ref()
            .unwrap()
            .valid,
        "other-tab center cannot swap"
    );
    let edge = (target.right() - 1, target.y + target.height / 2);
    let pending = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), edge);
    complete_layout(&mut state, &pending, destination);
    let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), edge);
    assert!(released.actions.iter().any(|action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(&request.method, Method::PaneMove(params) if params.pane_id == "pane_2"
            && matches!(&params.destination, PaneMoveDestination::TabPlacement { tab_id, target_pane_id, placement: PanePlacement::Right, .. }
                if tab_id == "tab_2" && target_pane_id == "pane_3")))));
}

#[test]
fn closed_source_and_restarted_server_cancel_without_layout_mutation() {
    for restart in [false, true] {
        let mut state = pair_state();
        press_second(&mut state);
        let target = state.hits.panes[0].rect;
        let point = (target.x + target.width / 2, target.y + target.height / 2);
        mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
        let mut snapshot = state.snapshot.as_deref().unwrap().clone();
        if restart {
            snapshot.boot_id = "boot-restarted".into();
        } else {
            snapshot.panes.retain(|pane| pane.pane_id != "pane_2");
        }
        state.set_snapshot(Box::new(snapshot));
        let tick = state.tick_pane_drag(Instant::now());
        assert!(tick.actions.is_empty());
        assert!(state.pane_drag.is_none());
    }
}

#[test]
fn too_small_projection_is_not_submitted() {
    let mut state = pair_state();
    press_second(&mut state);
    let target = state.hits.panes[0].rect;
    let point = (target.x + target.width / 2, target.bottom() - 1);
    let pending = mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
    state.last_composed_size = Some((120, 4));
    complete_layout(&mut state, &pending, pair_layout());
    assert!(
        !state
            .pane_drag
            .as_ref()
            .unwrap()
            .preview
            .as_ref()
            .unwrap()
            .valid
    );
    let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point);
    assert!(!released.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::PaneMove(_) | Method::PaneSwap(_)))
    ));
}

#[test]
fn reconnect_generation_cancels_drop_and_never_restores_old_focus() {
    for release in [false, true] {
        let mut state = pair_state();
        let snapshot = state.snapshot.clone().unwrap();
        state.apply_active_snapshot(snapshot, Some(1));
        show(&mut state, &pair_layout());
        press_second(&mut state);
        let target = state.hits.panes[0].rect;
        let point = (target.x + target.width / 2, target.y + target.height / 2);
        mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
        let origin = state.pane_drag.as_ref().unwrap().origin.clone();
        assert!(state.pane_drag.as_ref().unwrap().action.is_some());

        // A reconnect can reuse the same boot, pane IDs and projection revision.
        // It must still retire the gesture authored by the previous connection.
        let snapshot = state.snapshot.clone().unwrap();
        state.apply_active_snapshot(snapshot, Some(2));
        show(&mut state, &pair_layout());
        let result = if release {
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point)
        } else {
            state.tick_pane_drag(Instant::now())
        };
        assert!(result.actions.is_empty());
        assert!(state.pane_drag.is_none());
        let mut outcome = ClientShellInput::default();
        state.restore_pane_drag_origin(&origin, &mut outcome);
        assert!(outcome.actions.is_empty());
    }
}

#[test]
fn mismatched_surface_generation_cannot_start_or_submit_a_pane_drag() {
    let mut state = pair_state();
    let hit = state.hits.panes[1].clone();
    let press = (hit.rect.x + 2, hit.rect.y);
    state.pane_surface_generation = Some(42);
    assert!(state.pane_drag_title_at(press).is_none());

    state.pane_surface_generation = state.active_snapshot_generation;
    press_second(&mut state);
    let target = state.hits.panes[0].rect;
    let point = (target.x + target.width / 2, target.y + target.height / 2);
    mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), point);
    assert!(state.pane_drag.as_ref().unwrap().action.is_some());
    state.pane_surface_generation = Some(42);
    let released = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), point);
    assert!(!released.actions.iter().any(
        |action| matches!(action, ClientShellAction::Endpoint { request, .. }
        if matches!(request.method, Method::PaneMove(_) | Method::PaneSwap(_)))
    ));
    assert!(state.pane_drag.is_none());
}

#[test]
fn ctrl_hover_during_pane_drag_does_not_request_terminal_links() {
    let mut state = pair_state();
    press_second(&mut state);
    let target = state.hits.panes[0].inner_rect;
    let outcome =
        state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: target.x + target.width / 2,
            row: target.y + target.height / 2,
            modifiers: KeyModifiers::CONTROL,
        })]);
    assert!(state.pane_drag.is_some());
    assert!(state.link_hover.is_none());
    assert!(outcome.actions.is_empty());
}

#[test]
fn drag_preview_only_occludes_its_drawn_border_and_preserves_other_graphics() {
    use crate::protocol::{
        SurfaceGraphicsAsset, SurfaceGraphicsAssetKey, SurfaceGraphicsFormat,
        SurfaceGraphicsPlacement, SurfaceGraphicsScene, SurfaceGraphicsSource,
        SurfaceGraphicsTarget,
    };

    let mut state = pair_state();
    press_second(&mut state);
    let area = state.layout(120, 40).pane_surface;
    let rect = Rect::new(area.x + 5, area.y + 5, 16, 8);
    let covered = [
        (rect.x + 3, rect.y),
        (rect.x + 3, rect.bottom() - 1),
        (rect.x, rect.y + 3),
        (rect.right() - 1, rect.y + 3),
    ];
    let visible = [
        (rect.x + 3, rect.y + 3),
        (state.hits.panes[1].inner_rect.x + 3, rect.y + 3),
    ];
    let mut scene = SurfaceGraphicsScene::default();
    for (index, &(x, y)) in covered.iter().chain(&visible).enumerate() {
        let id = index as u32 + 1;
        let key = SurfaceGraphicsAssetKey {
            source: SurfaceGraphicsSource::Terminal {
                target: SurfaceGraphicsTarget::Pane {
                    pane_id: "pane_1".into(),
                },
                image_id: id,
            },
            image_width: 1,
            image_height: 1,
            format: SurfaceGraphicsFormat::Rgba,
            data_len: 4,
            data_fingerprint: u64::from(id),
        };
        scene.assets.push(SurfaceGraphicsAsset {
            key: key.clone(),
            data: vec![1, 2, 3, 4],
        });
        scene.placements.push(SurfaceGraphicsPlacement {
            asset: key,
            logical_placement_id: id,
            x: x - area.x,
            y: y - area.y,
            cols: 1,
            rows: 1,
            source_x: 0,
            source_y: 0,
            source_width: 1,
            source_height: 1,
            x_offset: 0,
            y_offset: 0,
            z: 0,
            scrollback_offset: 0,
        });
    }
    state.graphics.set_scene(scene);
    let placed = |bytes: &[u8], (x, y): (u16, u16)| {
        String::from_utf8_lossy(bytes).contains(&format!("\x1b[{};{}H", y + 1, x + 1))
    };
    let original = state.compose(120, 40).unwrap();
    let original_graphics = original.graphics.into_inline_bytes();
    for &point in covered.iter().chain(&visible) {
        assert!(placed(&original_graphics, point));
    }
    state.pane_drag.as_mut().unwrap().preview = Some(DropPreview {
        rects: vec![rect],
        label: "move below",
        valid: true,
    });
    let preview = state.compose(120, 40).unwrap();
    let preview_graphics = preview.graphics.into_inline_bytes();
    for point in covered {
        assert!(!placed(&preview_graphics, point), "covered={point:?}");
    }
    for point in visible {
        assert!(placed(&preview_graphics, point), "visible={point:?}");
    }
    state.cancel_pane_drag(&mut ClientShellInput::default());
    let restored = state.compose(120, 40).unwrap();
    let restored_graphics = restored.graphics.into_inline_bytes();
    for &point in covered.iter().chain(&visible) {
        assert!(placed(&restored_graphics, point));
    }
    assert!(!String::from_utf8_lossy(&restored_graphics).contains("a=t"));
}
