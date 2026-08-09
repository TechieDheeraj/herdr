use super::*;

/// A desktop click belongs to the endpoint boot that originally emitted it.
/// Pane IDs alone are not unique across machines or restarted servers.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(not(windows), test))]
pub(crate) struct ClientNotificationTarget {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) boot_id: String,
    pub(crate) pane_id: String,
}

impl ClientShellState {
    #[cfg(not(windows))]
    pub(super) fn notification_target(
        &self,
        endpoint_id: &ClientEndpointId,
        event: &SemanticNotification,
    ) -> Option<ClientNotificationTarget> {
        Some(ClientNotificationTarget {
            endpoint_id: endpoint_id.clone(),
            boot_id: self.endpoint_boot_id(endpoint_id)?.to_owned(),
            pane_id: event.pane_id.clone()?,
        })
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn notification_target_exists(&self, target: &ClientNotificationTarget) -> bool {
        self.endpoint_is_online(&target.endpoint_id)
            && self.endpoints.iter().any(|endpoint| {
                endpoint.endpoint_id == target.endpoint_id
                    && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                        snapshot.boot_id == target.boot_id
                            && snapshot
                                .panes
                                .iter()
                                .any(|pane| pane.pane_id == target.pane_id)
                    })
            })
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn focus_notification_target(
        &mut self,
        target: &ClientNotificationTarget,
    ) -> Option<ClientShellInput> {
        if !self.notification_target_exists(target) {
            return None;
        }
        let mut outcome = ClientShellInput::default();
        self.focus_or_activate(
            target.endpoint_id.clone(),
            ClientEndpointFocusTarget::Pane(target.pane_id.clone()),
            &mut outcome,
        );
        Some(outcome)
    }

    /// The host application can be activated only once the target pane's
    /// coherent snapshot and surface have been composed and presented.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn notification_target_is_presented(
        &self,
        target: &ClientNotificationTarget,
    ) -> bool {
        self.active_endpoint_id == target.endpoint_id
            && self.pane_surface_generation == self.active_snapshot_generation
            && self
                .snapshot
                .as_deref()
                .zip(self.pane_surface.as_ref())
                .is_some_and(|(snapshot, surface)| {
                    snapshot.boot_id == target.boot_id
                        && surface.boot_id == target.boot_id
                        && surface.projection_revision == snapshot.revision
                        && snapshot.focused_pane_id.as_deref() == Some(target.pane_id.as_str())
                        && surface
                            .panes
                            .iter()
                            .any(|pane| pane.pane_id == target.pane_id && pane.focused)
                })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_click_rejects_restarted_or_removed_panes() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let snapshot = super::super::tests::snapshot();
        let mut target = ClientNotificationTarget {
            endpoint_id: ClientEndpointId::Local,
            boot_id: snapshot.boot_id.clone(),
            pane_id: snapshot.panes[0].pane_id.clone(),
        };
        state.set_snapshot(Box::new(snapshot));
        assert!(state.notification_target_exists(&target));
        target.boot_id = "an-earlier-server".into();
        assert!(state.focus_notification_target(&target).is_none());
        target.boot_id = state
            .endpoint_boot_id(&ClientEndpointId::Local)
            .unwrap()
            .into();
        target.pane_id = "removed-pane".into();
        assert!(state.focus_notification_target(&target).is_none());
    }

    #[test]
    fn desktop_click_selects_the_original_machine_even_when_pane_ids_match() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        let snapshot = super::super::tests::snapshot();
        state.set_snapshot(Box::new(snapshot.clone()));
        let remote = SavedSshEndpoint::new("Build", "build", "main").unwrap();
        let remote_id = ClientEndpointId::Ssh(remote.id.clone());
        state.set_endpoint_catalog(&[remote]);
        state.set_endpoint_status(&remote_id, ClientEndpointStatus::Online);
        state.set_endpoint_snapshot(&remote_id, Box::new(snapshot.clone()));
        let target = ClientNotificationTarget {
            endpoint_id: remote_id.clone(),
            boot_id: snapshot.boot_id,
            pane_id: "pane_1".into(),
        };

        let outcome = state.focus_notification_target(&target).unwrap();
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(ClientEndpointFocusTarget::Pane(pane_id)),
        }] if *endpoint_id == remote_id && pane_id == "pane_1")
        );
        assert!(!state.notification_target_is_presented(&target));
        assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    }
}
