use super::*;
use crate::client::shell::row_colors::{lookup, RowColorTarget};
use crate::sidebar_color::{SidebarRowColor, SIDEBAR_COLOR_PRESETS};

fn text(frame: &FrameData) -> String {
    frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect()
}

#[test]
fn health_check_uses_selected_machine_metrics_and_drag_is_client_local() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut local = snapshot();
    local.health.cpu_usage_percentage = Some(23);
    local.health.temperature_celsius = Some(47);
    local.health.battery = Some(crate::health::BatteryHealth {
        percentage: 81,
        charging: Some(true),
    });
    local.health.ram =
        crate::health::ResourceUsage::new(8 * 1024 * 1024 * 1024, 16 * 1024 * 1024 * 1024);
    local.health.disk =
        crate::health::ResourceUsage::new(125 * 1024 * 1024 * 1024, 500 * 1024 * 1024 * 1024);
    state.set_snapshot(Box::new(local.clone()));
    state.set_pane_surface(surface());
    let frame = state.compose(120, 30).expect("local health frame");
    assert!(text(&frame).contains("23% · 47°C"));
    assert!(text(&frame).contains("81% ↑"));
    let divider = state.hits.sidebar_health_divider;
    let outcome = state.handle_raw_events(vec![
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: divider.x + 2,
            row: divider.y,
            modifiers: KeyModifiers::empty(),
        }),
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: divider.x + 2,
            row: 17,
            modifiers: KeyModifiers::empty(),
        }),
        RawInputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: divider.x + 2,
            row: 17,
            modifiers: KeyModifiers::empty(),
        }),
    ]);
    assert!(outcome.requests.is_empty());
    assert!(!outcome.resize);
    state.compose(120, 30).expect("resized health frame");
    assert_eq!(state.hits.sidebar_health_divider.y, 17);

    let profile = crate::client::endpoint::SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("profile"),
        label: "Linux".into(),
        target: "dev@linux".into(),
        session: "test".into(),
        enabled: true,
    };
    let endpoint = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
    let mut remote = local;
    remote.boot_id = "remote-boot".into();
    remote.health.cpu_usage_percentage = Some(72);
    remote.health.temperature_celsius = Some(59);
    state.set_endpoint_snapshot(&endpoint, Box::new(remote));
    assert!(state.activate_endpoint_projection(&endpoint));
    let mut remote_surface = surface();
    remote_surface.boot_id = "remote-boot".into();
    state.set_pane_surface(remote_surface);
    let frame = state.compose(120, 30).expect("remote health frame");
    assert!(text(&frame).contains("72% · 59°C"));
    assert!(!text(&frame).contains("23% · 47°C"));
}

#[test]
fn workspace_and_tab_color_picker_keeps_swatches_separate_from_actions() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let original = state
        .compose(120, 30)
        .expect("original colors")
        .to_ratatui_buffer()
        .expect("buffer");
    state.open_workspace_context_menu("ws_1".into(), 2, 3);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("menu");
    };
    let index = menu
        .items()
        .iter()
        .position(|item| item.label == "Color")
        .expect("Color option");
    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(index, &mut outcome);
    state.compose(120, 30).expect("color dialog");
    let mut overlay_buffer = Buffer::empty(Rect::new(0, 0, 120, 30));
    let rendered = render::render_client_overlay(
        &mut overlay_buffer,
        state.overlay.as_ref().expect("color picker"),
        state.snapshot.as_deref().expect("snapshot"),
        &state.endpoints,
        &state.active_endpoint_id,
        &state.config.keybinds,
        &state.config.palette,
    )
    .expect("rendered color picker");
    assert!(
        !rendered.area.is_empty(),
        "picker must occlude terminal images"
    );
    for rect in
        rendered
            .color_swatches
            .iter()
            .chain([&rendered.primary, &rendered.clear, &rendered.cancel])
    {
        assert_eq!(rect.intersection(rendered.area), *rect);
    }
    assert_eq!(state.hits.color_swatches.len(), 12);
    assert!(state
        .hits
        .color_swatches
        .iter()
        .all(|rect| rect.bottom() < state.hits.overlay_primary.y));
    state.insert_overlay_text("#f3c");
    state.row_color_key(
        &crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty()),
        &mut outcome,
    );
    let color = SidebarRowColor::new(255, 51, 204);
    assert_eq!(
        lookup(&state.workspace_colors, &ClientEndpointId::Local, "ws_1"),
        Some(color)
    );
    assert!(outcome.requests.is_empty());
    let colored = state
        .compose(120, 30)
        .expect("colored workspace")
        .to_ratatui_buffer()
        .expect("buffer");
    let workspace = state.hits.workspaces[0].rect;
    for y in workspace.y..workspace.bottom() {
        for x in workspace.x..workspace.right() {
            assert_eq!(colored[(x, y)].bg, color.ratatui());
            assert_eq!(colored[(x, y)].fg, color.contrast());
            assert!(!colored[(x, y)].modifier.contains(Modifier::DIM));
        }
    }
    state.open_row_color_picker(RowColorTarget::Workspace("ws_1".into()));
    state.save_row_color(true, &mut outcome);
    assert_eq!(
        lookup(&state.workspace_colors, &ClientEndpointId::Local, "ws_1"),
        None
    );
    let cleared = state
        .compose(120, 30)
        .expect("restored colors")
        .to_ratatui_buffer()
        .expect("buffer");
    for y in workspace.y..workspace.bottom() {
        for x in workspace.x..workspace.right() {
            assert_eq!(cleared[(x, y)].style(), original[(x, y)].style());
        }
    }

    state.open_tab_context_menu("tab_1".into(), 30, 0);
    let Some(ClientShellOverlay::ContextMenu(menu)) = state.overlay.as_ref() else {
        panic!("tab menu");
    };
    let index = menu
        .items()
        .iter()
        .position(|item| item.label == "Color")
        .expect("tab Color");
    state.activate_context_menu_item(index, &mut outcome);
    state.compose(120, 30).expect("tab color picker");
    let swatch = state.hits.color_swatches[11];
    state.row_color_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: swatch.x,
            row: swatch.y,
            modifiers: KeyModifiers::empty(),
        },
        &mut outcome,
    );
    assert_eq!(
        lookup(&state.tab_colors, &ClientEndpointId::Local, "tab_1"),
        Some(SIDEBAR_COLOR_PRESETS[11].1)
    );
}

#[test]
fn default_agent_rows_keep_single_tab_pane_and_remote_directory_context() {
    let mut projected = snapshot();
    projected.panes[0].label = Some("pane 1".into());
    projected.panes[0].cwd = Some("/home/dev/projects/service/".into());
    projected.panes[0].foreground_cwd = Some("/home/dev/projects/service/subprocess".into());
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: None,
        agent: Some("codex".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: true,
    });
    let config = ClientShellConfig::from_config(&Config::default());
    let rows = super::super::agent_sidebar::agent_rows(&projected, &config, None);
    let row = &rows[0];
    assert!(row.rows[0]
        .iter()
        .any(|token| token.kind == crate::ui::ResolvedTokenKind::Tab("1".into())));
    assert!(row.rows[0]
        .iter()
        .any(|token| token.kind == crate::ui::ResolvedTokenKind::Pane("pane 1".into())));
    assert!(row.rows[1]
        .iter()
        .any(|token| token.kind == crate::ui::ResolvedTokenKind::Directory("service".into())));
    assert!(row.rows[1]
        .iter()
        .any(|token| token.kind == crate::ui::ResolvedTokenKind::Agent("codex".into())));
}

#[test]
fn expanded_machine_sidebar_keeps_row_gaps_and_trailing_blank_above_health() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.config.spaces.row_gap = 1;
    let mut local = snapshot();
    let template = local.workspaces[0].clone();
    local.workspaces = (1..=12)
        .map(|number| ClientShellWorkspace {
            workspace_id: format!("ws_{number}"),
            number,
            label: format!("workspace-{number}"),
            branch: None,
            focused: number == 1,
            ..template.clone()
        })
        .collect();
    state.set_snapshot(Box::new(local));
    state.set_pane_surface(surface());
    let profile = crate::client::endpoint::SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("profile"),
        label: "Linux".into(),
        target: "dev@linux".into(),
        session: "test".into(),
        enabled: true,
    };
    let endpoint = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&endpoint, ClientEndpointStatus::Online);
    let mut remote = snapshot();
    remote.boot_id = "remote-boot".into();
    remote.workspaces[0].branch = None;
    state.set_endpoint_snapshot(&endpoint, Box::new(remote));

    state.compose(120, 40).expect("top of machine list");
    let local_rows = state
        .hits
        .workspaces
        .iter()
        .filter(|hit| hit.endpoint_id.is_local())
        .collect::<Vec<_>>();
    assert!(local_rows.len() >= 2);
    for pair in local_rows.windows(2) {
        assert_eq!(pair[1].rect.y, pair[0].rect.bottom() + 1);
    }
    assert!(state.hits.workspace_max_scroll > 0);
    state.workspace_scroll = usize::MAX;
    state.compose(120, 40).expect("bottom of machine list");
    let last = state.hits.workspaces.last().expect("last workspace");
    assert_eq!(last.endpoint_id, endpoint);
    assert!(
        last.rect.bottom() < state.hits.workspace_body.bottom(),
        "blank workspace creation target remains visible at the bottom"
    );
    assert!(state.hits.workspace_body.bottom() < state.hits.sidebar_health_divider.y);
}
