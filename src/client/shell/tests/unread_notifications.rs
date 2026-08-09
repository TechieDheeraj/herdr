use super::*;

fn idle_agent(pane_id: &str) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: Some("codex".into()),
        agent: Some("codex".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: pane_id == "pane_1",
    }
}

#[test]
fn agent_right_click_marks_unread_without_changing_focus_or_offering_color() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut snapshot = snapshot();
    snapshot.agents.push(idle_agent("pane_1"));
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state.compose(120, 40).unwrap();
    let rect = state.hits.agents[0].0;
    let right_click = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: rect.x + 1,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(right_click.actions.is_empty());
    let Some(ClientShellOverlay::ContextMenu(menu)) = &state.overlay else {
        panic!("expected agent context menu");
    };
    assert_eq!(
        menu.items()
            .iter()
            .map(|item| item.label)
            .collect::<Vec<_>>(),
        vec!["Unread"]
    );
    let mut outcome = ClientShellInput::default();
    state.activate_context_menu_item(0, &mut outcome);
    state.compose(120, 40).unwrap();
    assert_eq!(
        state.snapshot.as_ref().unwrap().agents[0].agent_status,
        AgentStatus::Done
    );
    assert_eq!(
        state.snapshot.as_ref().unwrap().focused_pane_id.as_deref(),
        Some("pane_1")
    );
    assert!(outcome.actions.is_empty());

    state.push_endpoint_method(
        crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget {
            pane_id: "pane_1".into(),
        }),
        &mut outcome,
    );
    assert_eq!(
        state.snapshot.as_ref().unwrap().agents[0].agent_status,
        AgentStatus::Idle
    );
}

#[test]
fn same_tab_unfocused_pane_keeps_its_external_notification_and_click_target() {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.toast_delivery = crate::config::ToastDelivery::System;
    config.toast_delay_seconds = 0;
    let mut state = ClientShellState::new(config);
    state.outer_focused = Some(true);
    let mut snapshot = snapshot();
    snapshot.agents.push(idle_agent("pane_2"));
    let mut sibling = snapshot.panes[0].clone();
    sibling.pane_id = "pane_2".into();
    sibling.focused = false;
    snapshot.panes.push(sibling);
    state.set_snapshot(Box::new(snapshot));
    state.mark_agent_unread(&ClientEndpointId::Local, "pane_2");

    let (effects, _) = state.receive_notification(
        &ClientEndpointId::Local,
        SemanticNotification {
            kind: SemanticNotificationKind::Finished,
            title: "codex finished".into(),
            body: None,
            sound: None,
            agent: Some("codex".into()),
            workspace_id: Some("ws_1".into()),
            tab_id: Some("tab_1".into()),
            pane_id: Some("pane_2".into()),
            position: None,
        },
        std::time::Instant::now(),
    );
    assert!(
        matches!(effects.as_slice(), [ClientShellNotificationEffect::System {
        target: Some(target), ..
    }] if target.endpoint_id == ClientEndpointId::Local && target.pane_id == "pane_2" && target.boot_id == "boot-1")
    );
}

#[test]
fn native_activation_waits_for_the_focused_pane_surface_revision() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let mut snapshot = snapshot();
    let target = ClientNotificationTarget {
        endpoint_id: ClientEndpointId::Local,
        boot_id: snapshot.boot_id.clone(),
        pane_id: "pane_1".into(),
    };
    state.set_snapshot(Box::new(snapshot.clone()));
    assert!(!state.notification_target_is_presented(&target));
    state.set_pane_surface(surface());
    assert!(state.notification_target_is_presented(&target));
    snapshot.revision += 1;
    state.set_snapshot(Box::new(snapshot));
    assert!(!state.notification_target_is_presented(&target));
    let mut coherent = surface();
    coherent.projection_revision += 1;
    state.set_pane_surface(coherent);
    assert!(state.notification_target_is_presented(&target));
}

#[test]
fn manual_unread_participates_in_selected_machine_seen_filter() {
    use crate::api::schema::{
        AgentViewBuiltinField, AgentViewField, AgentViewFilter, AgentViewSetParams, AgentViewValue,
    };

    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let profile = SavedSshEndpoint::new("Build", "build", "main").unwrap();
    let remote_id = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
    let mut local = snapshot();
    local.agents.push(idle_agent("pane_1"));
    state.set_snapshot(Box::new(local.clone()));
    state.set_pane_surface(surface());
    local.boot_id = "remote-boot".into();
    state.set_endpoint_snapshot(&remote_id, Box::new(local));
    state.set_test_endpoint_agent_view(
        &ClientEndpointId::Local,
        Some(AgentViewSetParams {
            source: "test.views".into(),
            label: Some("unread".into()),
            filter: Some(AgentViewFilter::Eq {
                field: AgentViewField::Builtin(AgentViewBuiltinField::Seen),
                value: AgentViewValue::Bool(false),
            }),
            sort: Vec::new(),
        }),
    );
    let unread_targets = |state: &ClientShellState| {
        super::super::aggregate_navigation::aggregate_agent_rows(
            &state.endpoints,
            &state.active_endpoint_id,
            state.config.agent_panel_sort,
        )
        .into_iter()
        .map(|row| (row.endpoint.endpoint_id.clone(), row.agent.pane_id.clone()))
        .collect::<Vec<_>>()
    };
    assert!(unread_targets(&state).is_empty());

    assert!(state.mark_agent_unread(&remote_id, "pane_1"));
    assert_eq!(
        unread_targets(&state),
        vec![(remote_id.clone(), "pane_1".into())]
    );
    assert!(state.mark_agent_unread(&ClientEndpointId::Local, "pane_1"));
    state.compose(120, 40).unwrap();
    assert_eq!(unread_targets(&state).len(), 2);

    // Explicitly acknowledging Local must not acknowledge the identically named remote pane.
    assert!(state.acknowledge_explicit_agent_focus("pane_1"));
    assert_eq!(unread_targets(&state), vec![(remote_id, "pane_1".into())]);
}

#[test]
fn agent_row_keeps_single_tab_pane_and_initial_directory_labels() {
    use crate::ui::ResolvedTokenKind;

    let config = ClientShellConfig::from_config(&Config::default());
    let mut snapshot = snapshot();
    snapshot.agents.push(idle_agent("pane_1"));
    snapshot.panes[0].label = Some("pane 1".into());
    snapshot.panes[0].cwd = Some("/repo/launch-dir/".into());
    snapshot.panes[0].foreground_cwd = Some("/repo/later-dir".into());
    let row = super::super::agent_sidebar::agent_row(&snapshot, "pane_1", &config, None)
        .expect("single-tab agent row");
    let label_rows = row
        .rows
        .iter()
        .map(|tokens| {
            tokens
                .iter()
                .map(|token| token.kind.clone())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        label_rows,
        vec![
            vec![
                ResolvedTokenKind::StateIcon,
                ResolvedTokenKind::Workspace("client-shell".into()),
                ResolvedTokenKind::Tab("1".into()),
                ResolvedTokenKind::Pane("pane 1".into()),
            ],
            vec![
                ResolvedTokenKind::Directory("launch-dir".into()),
                ResolvedTokenKind::Agent("codex".into()),
            ],
        ]
    );
}

#[test]
fn native_activation_waits_for_the_reconnected_surface_generation() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    let snapshot = snapshot();
    let target = ClientNotificationTarget {
        endpoint_id: ClientEndpointId::Local,
        boot_id: snapshot.boot_id.clone(),
        pane_id: "pane_1".into(),
    };
    state.cache_endpoint_snapshot_inactive_for_generation(
        &ClientEndpointId::Local,
        1,
        Box::new(snapshot.clone()),
    );
    assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
    state.set_pane_surface(surface());
    assert!(state.notification_target_is_presented(&target));

    // The server boot and projection revision can be identical after reconnect.
    state.cache_endpoint_snapshot_inactive_for_generation(
        &ClientEndpointId::Local,
        2,
        Box::new(snapshot),
    );
    assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
    assert!(!state.notification_target_is_presented(&target));
    state.set_pane_surface(surface());
    assert!(state.notification_target_is_presented(&target));
}
