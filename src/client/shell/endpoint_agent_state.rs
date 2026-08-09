use std::collections::{HashMap, HashSet};

use crate::api::schema::AgentStatus;
use crate::protocol::{ClientShellAgent, ClientShellSnapshot, PaneSurfaceFrame};

#[derive(Clone, Debug, Default)]
pub(super) struct EndpointAgentPresentation {
    boot_id: Option<String>,
    acknowledged: HashMap<String, u64>,
    completed: HashMap<String, u64>,
    working: HashSet<String>,
    pending_completions: Option<(
        Option<u64>,
        crate::protocol::endpoint::EndpointAgentCompletions,
    )>,
    manual_unread: HashMap<String, u64>,
    last_presented_focus: Option<String>,
}

impl EndpointAgentPresentation {
    pub(super) fn receive_completions(
        &mut self,
        generation: Option<u64>,
        completions: crate::protocol::endpoint::EndpointAgentCompletions,
    ) {
        if self
            .pending_completions
            .as_ref()
            .is_some_and(|(current_generation, current)| {
                *current_generation == generation
                    && current.boot_id == completions.boot_id
                    && current.revision >= completions.revision
            })
        {
            return;
        }
        self.pending_completions = Some((generation, completions));
    }

    pub(super) fn leave_surface(&mut self) {
        self.last_presented_focus = None;
    }

    #[cfg(test)]
    pub(super) fn project_snapshot(&mut self, snapshot: &mut ClientShellSnapshot) {
        self.project_snapshot_for_generation(snapshot, None);
    }

    pub(super) fn project_snapshot_for_generation(
        &mut self,
        snapshot: &mut ClientShellSnapshot,
        generation: Option<u64>,
    ) {
        if self.boot_id.as_deref() != Some(snapshot.boot_id.as_str()) {
            self.boot_id = Some(snapshot.boot_id.clone());
            self.acknowledged.clear();
            self.completed.clear();
            self.working.clear();
            self.manual_unread.clear();
            self.last_presented_focus = None;
            self.acknowledged.extend(
                snapshot
                    .agents
                    .iter()
                    .map(|agent| (agent.pane_id.clone(), agent.state_change_seq)),
            );
        }
        let pane_ids: HashSet<&str> = snapshot
            .agents
            .iter()
            .map(|agent| agent.pane_id.as_str())
            .collect();
        self.acknowledged
            .retain(|pane_id, _| pane_ids.contains(pane_id.as_str()));
        self.completed
            .retain(|pane_id, _| pane_ids.contains(pane_id.as_str()));
        self.working
            .retain(|pane_id| pane_ids.contains(pane_id.as_str()));
        let completions = self
            .pending_completions
            .take()
            .filter(|(received_generation, projection)| {
                *received_generation == generation
                    && projection.boot_id == snapshot.boot_id
                    && projection.revision == snapshot.revision
            })
            .map(|(_, projection)| projection.completions);
        self.manual_unread.retain(|pane_id, sequence| {
            snapshot
                .agents
                .iter()
                .any(|agent| &agent.pane_id == pane_id && agent.state_change_seq == *sequence)
        });
        for agent in &mut snapshot.agents {
            match agent.agent_status {
                AgentStatus::Working => {
                    self.working.insert(agent.pane_id.clone());
                    self.completed.remove(&agent.pane_id);
                }
                AgentStatus::Blocked => {
                    self.completed.remove(&agent.pane_id);
                }
                AgentStatus::Idle | AgentStatus::Done => {
                    let observed_work = self.working.remove(&agent.pane_id);
                    let completed = completions.as_ref().map_or_else(
                        || {
                            observed_work
                                || self.completed.get(&agent.pane_id)
                                    == Some(&agent.state_change_seq)
                        },
                        |completions| {
                            completions.get(&agent.pane_id) == Some(&agent.state_change_seq)
                        },
                    );
                    if completed {
                        self.completed
                            .insert(agent.pane_id.clone(), agent.state_change_seq);
                    } else {
                        self.completed.remove(&agent.pane_id);
                    }
                }
                _ => {
                    self.working.remove(&agent.pane_id);
                    self.completed.remove(&agent.pane_id);
                }
            }
            agent.agent_status = self.projected_status(agent);
        }
        project_aggregate_status(snapshot);
    }

    pub(super) fn acknowledge_surface(
        &mut self,
        snapshot: &mut ClientShellSnapshot,
        surface: &PaneSurfaceFrame,
        outer_focused: Option<bool>,
    ) -> bool {
        if outer_focused == Some(false)
            || self.boot_id.as_deref() != Some(surface.boot_id.as_str())
            || snapshot.boot_id != surface.boot_id
            || snapshot.revision != surface.projection_revision
        {
            return false;
        }

        let focused = surface.panes.iter().find(|pane| {
            pane.focused && snapshot.focused_pane_id.as_deref() == Some(pane.pane_id.as_str())
        });
        let focus_changed =
            self.last_presented_focus.as_deref() != focused.map(|pane| pane.pane_id.as_str());
        if focus_changed {
            self.last_presented_focus = focused.map(|pane| pane.pane_id.clone());
        }
        let mut changed = false;
        if let Some(pane) = focused {
            if focus_changed {
                changed |= self.manual_unread.remove(&pane.pane_id).is_some();
            }
            if !self.manual_unread.contains_key(&pane.pane_id) {
                if let Some(agent) = snapshot
                    .agents
                    .iter()
                    .find(|agent| agent.pane_id == pane.pane_id)
                {
                    let acknowledged = self.acknowledged.entry(agent.pane_id.clone()).or_default();
                    if *acknowledged < agent.state_change_seq {
                        *acknowledged = agent.state_change_seq;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            for agent in &mut snapshot.agents {
                agent.agent_status = self.projected_status(agent);
            }
            project_aggregate_status(snapshot);
        }
        changed
    }

    pub(super) fn seen(&self, agent: &ClientShellAgent) -> bool {
        !self.manual_unread.contains_key(&agent.pane_id)
            && self.completed.get(&agent.pane_id).is_none_or(|completion| {
                self.acknowledged
                    .get(&agent.pane_id)
                    .is_some_and(|sequence| sequence >= completion)
            })
    }

    pub(super) fn mark_unread(
        &mut self,
        snapshot: &mut ClientShellSnapshot,
        pane_id: &str,
    ) -> bool {
        let Some(agent) = snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)
        else {
            return false;
        };
        self.manual_unread
            .insert(pane_id.to_owned(), agent.state_change_seq);
        self.last_presented_focus = snapshot.focused_pane_id.clone();
        self.reproject_statuses(snapshot);
        true
    }

    pub(super) fn acknowledge_explicit_focus(
        &mut self,
        snapshot: &mut ClientShellSnapshot,
        pane_id: &str,
    ) -> bool {
        // A click on an already focused pane is an explicit acknowledgement too.
        // Background targets wait for their coherent focused surface instead.
        if snapshot.focused_pane_id.as_deref() != Some(pane_id)
            || self.manual_unread.remove(pane_id).is_none()
        {
            return false;
        }
        self.reproject_statuses(snapshot);
        true
    }

    fn reproject_statuses(&self, snapshot: &mut ClientShellSnapshot) {
        for agent in &mut snapshot.agents {
            agent.agent_status = self.projected_status(agent);
        }
        project_aggregate_status(snapshot);
    }

    fn projected_status(&self, agent: &ClientShellAgent) -> AgentStatus {
        match agent.agent_status {
            AgentStatus::Idle | AgentStatus::Done => {
                if self.seen(agent) {
                    AgentStatus::Idle
                } else {
                    AgentStatus::Done
                }
            }
            status => status,
        }
    }
}

fn project_aggregate_status(snapshot: &mut ClientShellSnapshot) {
    for tab in &mut snapshot.tabs {
        if let Some(status) = snapshot
            .agents
            .iter()
            .filter(|agent| agent.tab_id == tab.tab_id)
            .map(|agent| agent.agent_status)
            .max_by_key(|status| super::status_priority(*status))
        {
            tab.agent_status = status;
        }
    }
    for workspace in &mut snapshot.workspaces {
        if let Some(status) = snapshot
            .agents
            .iter()
            .filter(|agent| agent.workspace_id == workspace.workspace_id)
            .map(|agent| agent.agent_status)
            .max_by_key(|status| super::status_priority(*status))
        {
            workspace.agent_status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        endpoint::EndpointAgentCompletions, FrameData, PaneSurfacePane, SurfaceRect,
    };

    fn agent(status: AgentStatus, sequence: u64) -> ClientShellAgent {
        ClientShellAgent {
            pane_id: "agent-pane".into(),
            workspace_id: "workspace".into(),
            tab_id: "tab".into(),
            name: None,
            display_agent: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: status,
            state_change_seq: sequence,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: true,
        }
    }

    fn snapshot(status: AgentStatus, sequence: u64, revision: u64) -> ClientShellSnapshot {
        let mut snapshot = crate::client::shell::tests::snapshot();
        snapshot.boot_id = "endpoint-boot".into();
        snapshot.revision = revision;
        snapshot.focused_pane_id = Some("agent-pane".into());
        snapshot.agents = vec![agent(status, sequence)];
        snapshot
    }

    fn surface(revision: u64) -> PaneSurfaceFrame {
        let rect = SurfaceRect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        PaneSurfaceFrame {
            boot_id: "endpoint-boot".into(),
            projection_revision: revision,
            surface_revision: 1,
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: vec![PaneSurfacePane {
                pane_id: "agent-pane".into(),
                content_revision: 1,
                rect,
                inner_rect: rect,
                scrollbar_rect: None,
                scroll: None,
                focused: true,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }],
            splits: Vec::new(),
            popup: None,
            graphics: Default::default(),
        }
    }

    #[test]
    fn first_snapshot_establishes_an_idle_baseline_without_server_seen_authority() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut snapshot = snapshot(AgentStatus::Done, 4, 1);

        presentation.project_snapshot(&mut snapshot);

        assert_eq!(snapshot.agents[0].agent_status, AgentStatus::Idle);
    }

    fn assert_idle_sequence(states: &[(AgentStatus, u64)]) {
        let mut presentation = EndpointAgentPresentation::default();
        let mut projected = AgentStatus::Unknown;
        for (revision, &(status, seq)) in states.iter().enumerate() {
            let mut snapshot = snapshot(status, seq, revision as u64 + 1);
            presentation.project_snapshot(&mut snapshot);
            projected = snapshot.agents[0].agent_status;
        }
        assert_eq!(projected, AgentStatus::Idle, "{states:?}");
    }

    fn completions(boot: &str, revision: u64, seq: Option<u64>) -> EndpointAgentCompletions {
        EndpointAgentCompletions {
            boot_id: boot.into(),
            revision,
            completions: seq
                .map(|seq| ("agent-pane".into(), seq))
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn completion_guard_initial_unknown_to_idle_does_not_project_done() {
        assert_idle_sequence(&[(AgentStatus::Unknown, 0), (AgentStatus::Idle, 1)]);
    }

    #[test]
    fn completion_guard_new_idle_agent_is_not_completed_work() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut baseline = snapshot(AgentStatus::Idle, 0, 1);
        baseline.agents.clear();
        presentation.project_snapshot(&mut baseline);
        let mut ready = snapshot(AgentStatus::Idle, 1, 2);

        presentation.project_snapshot(&mut ready);

        assert_eq!(ready.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn completion_guard_session_rebind_does_not_project_done() {
        assert_idle_sequence(&[
            (AgentStatus::Idle, 4),
            (AgentStatus::Unknown, 5),
            (AgentStatus::Idle, 6),
        ]);
    }

    #[test]
    fn completion_guard_startup_blocker_does_not_project_done() {
        assert_idle_sequence(&[
            (AgentStatus::Unknown, 0),
            (AgentStatus::Blocked, 1),
            (AgentStatus::Idle, 2),
        ]);
    }

    #[test]
    fn completion_guard_distinguishes_coalesced_work_from_rebind() {
        for completion in [None, Some(6)] {
            let mut presentation = EndpointAgentPresentation::default();
            let mut baseline = snapshot(AgentStatus::Idle, 4, 1);
            presentation.project_snapshot(&mut baseline);
            presentation.receive_completions(None, completions(&baseline.boot_id, 2, completion));
            let mut settled = snapshot(AgentStatus::Idle, 6, 2);

            presentation.project_snapshot(&mut settled);

            let expected = if completion.is_some() {
                AgentStatus::Done
            } else {
                AgentStatus::Idle
            };
            assert_eq!(settled.agents[0].agent_status, expected);
            presentation.project_snapshot(&mut settled);
            assert_eq!(settled.agents[0].agent_status, expected);
        }
    }

    #[test]
    fn completion_guard_server_suppression_overrides_client_observed_work() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut baseline = snapshot(AgentStatus::Working, 1, 1);
        presentation.project_snapshot(&mut baseline);
        presentation.receive_completions(None, completions(&baseline.boot_id, 2, None));
        let mut settled = snapshot(AgentStatus::Idle, 2, 2);

        presentation.project_snapshot(&mut settled);

        assert_eq!(settled.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn completion_guard_companion_is_scoped_to_boot_revision_and_connection() {
        for (boot, revision, generation, seq) in [
            ("old-boot", 2, Some(1), 6),
            ("endpoint-boot", 1, Some(1), 6),
            ("endpoint-boot", 2, Some(0), 6),
            ("endpoint-boot", 2, Some(1), 5),
        ] {
            let mut presentation = EndpointAgentPresentation::default();
            let mut baseline = snapshot(AgentStatus::Idle, 4, 1);
            presentation.project_snapshot_for_generation(&mut baseline, Some(1));
            presentation.receive_completions(generation, completions(boot, revision, Some(seq)));
            let mut settled = snapshot(AgentStatus::Idle, 6, 2);

            presentation.project_snapshot_for_generation(&mut settled, Some(1));

            assert_eq!(settled.agents[0].agent_status, AgentStatus::Idle);
        }
    }

    #[test]
    fn manual_unread_preserves_pending_generation_scoped_completion() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut baseline = snapshot(AgentStatus::Idle, 4, 1);
        let mut other = agent(AgentStatus::Idle, 3);
        other.pane_id = "other-pane".into();
        baseline.agents.push(other.clone());
        presentation.project_snapshot_for_generation(&mut baseline, Some(7));
        presentation.receive_completions(Some(7), completions(&baseline.boot_id, 2, Some(6)));

        assert!(presentation.mark_unread(&mut baseline, "other-pane"));
        assert_eq!(baseline.agents[1].agent_status, AgentStatus::Done);
        assert!(presentation.pending_completions.is_some());
        let mut settled = snapshot(AgentStatus::Idle, 6, 2);
        settled.agents.push(other);
        presentation.project_snapshot_for_generation(&mut settled, Some(7));
        assert_eq!(settled.agents[0].agent_status, AgentStatus::Done);
        assert_eq!(settled.agents[1].agent_status, AgentStatus::Done);
        assert!(presentation.pending_completions.is_none());
    }

    #[test]
    fn completion_guard_new_boot_baselines_existing_completions() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut old = snapshot(AgentStatus::Working, 1, 1);
        old.boot_id = "old-boot".into();
        presentation.project_snapshot(&mut old);
        presentation.receive_completions(None, completions("endpoint-boot", 1, Some(6)));
        let mut restored = snapshot(AgentStatus::Idle, 6, 1);

        presentation.project_snapshot(&mut restored);

        assert_eq!(restored.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn unpresented_working_completion_projects_done() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut initial = snapshot(AgentStatus::Working, 4, 1);
        presentation.project_snapshot(&mut initial);
        let mut completed = snapshot(AgentStatus::Idle, 5, 2);

        presentation.project_snapshot(&mut completed);

        assert_eq!(completed.agents[0].agent_status, AgentStatus::Done);
    }

    #[test]
    fn coherent_presented_surface_acknowledges_completion() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut initial = snapshot(AgentStatus::Working, 4, 1);
        presentation.project_snapshot(&mut initial);
        let mut completed = snapshot(AgentStatus::Idle, 5, 2);
        presentation.project_snapshot(&mut completed);

        assert!(presentation.acknowledge_surface(&mut completed, &surface(2), Some(true)));
        assert_eq!(completed.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn clients_acknowledge_the_same_endpoint_completion_independently() {
        let mut viewing_client = EndpointAgentPresentation::default();
        let mut background_client = EndpointAgentPresentation::default();
        let mut initial_for_viewer = snapshot(AgentStatus::Working, 4, 1);
        let mut initial_for_background = initial_for_viewer.clone();
        viewing_client.project_snapshot(&mut initial_for_viewer);
        background_client.project_snapshot(&mut initial_for_background);
        let mut completed_for_viewer = snapshot(AgentStatus::Idle, 5, 2);
        let mut completed_for_background = completed_for_viewer.clone();
        viewing_client.project_snapshot(&mut completed_for_viewer);
        background_client.project_snapshot(&mut completed_for_background);

        assert!(viewing_client.acknowledge_surface(
            &mut completed_for_viewer,
            &surface(2),
            Some(true)
        ));

        assert_eq!(
            completed_for_viewer.agents[0].agent_status,
            AgentStatus::Idle
        );
        assert_eq!(
            completed_for_background.agents[0].agent_status,
            AgentStatus::Done
        );
    }

    #[test]
    fn stale_or_unfocused_surface_does_not_acknowledge_completion() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut initial = snapshot(AgentStatus::Working, 4, 1);
        presentation.project_snapshot(&mut initial);
        let mut completed = snapshot(AgentStatus::Idle, 5, 2);
        presentation.project_snapshot(&mut completed);

        assert!(!presentation.acknowledge_surface(&mut completed, &surface(1), Some(true)));
        assert!(!presentation.acknowledge_surface(&mut completed, &surface(2), Some(false)));
        assert_eq!(completed.agents[0].agent_status, AgentStatus::Done);
    }

    #[test]
    fn visible_sibling_completion_is_unread_until_its_pane_is_focused() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut initial = snapshot(AgentStatus::Working, 4, 1);
        let mut sibling = agent(AgentStatus::Working, 4);
        sibling.pane_id = "sibling".into();
        sibling.focused = false;
        initial.agents.push(sibling);
        presentation.project_snapshot(&mut initial);
        let mut completed = initial;
        completed.revision = 2;
        for agent in &mut completed.agents {
            agent.agent_status = AgentStatus::Idle;
            agent.state_change_seq = 5;
        }
        presentation.project_snapshot(&mut completed);
        let mut visible = surface(2);
        let mut sibling_surface = visible.panes[0].clone();
        sibling_surface.pane_id = "sibling".into();
        sibling_surface.focused = false;
        visible.panes.push(sibling_surface);

        assert!(presentation.acknowledge_surface(&mut completed, &visible, Some(true)));
        assert_eq!(completed.agents[0].agent_status, AgentStatus::Idle);
        assert_eq!(completed.agents[1].agent_status, AgentStatus::Done);

        completed.focused_pane_id = Some("sibling".into());
        visible.panes[0].focused = false;
        visible.panes[1].focused = true;
        assert!(presentation.acknowledge_surface(&mut completed, &visible, Some(true)));
        assert_eq!(completed.agents[1].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn manual_unread_survives_redraw_until_explicit_focus() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut snapshot = snapshot(AgentStatus::Idle, 0, 1);
        presentation.project_snapshot(&mut snapshot);
        assert!(presentation.mark_unread(&mut snapshot, "agent-pane"));
        assert_eq!(snapshot.agents[0].agent_status, AgentStatus::Done);
        assert!(!presentation.acknowledge_surface(&mut snapshot, &surface(1), Some(true)));
        assert_eq!(snapshot.agents[0].agent_status, AgentStatus::Done);
        assert!(presentation.acknowledge_explicit_focus(&mut snapshot, "agent-pane"));
        assert_eq!(snapshot.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn returning_to_a_machine_acknowledges_its_focused_manual_unread_pane() {
        let mut presentation = EndpointAgentPresentation::default();
        let mut snapshot = snapshot(AgentStatus::Idle, 0, 1);
        presentation.project_snapshot(&mut snapshot);
        presentation.mark_unread(&mut snapshot, "agent-pane");
        presentation.leave_surface();
        assert!(presentation.acknowledge_surface(&mut snapshot, &surface(1), Some(true)));
        assert_eq!(snapshot.agents[0].agent_status, AgentStatus::Idle);
    }
}
