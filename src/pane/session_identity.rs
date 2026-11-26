//! Passive identity from a CLI's configured session footer. Runs only when the
//! existing detection snapshot changes, outside rendering and PTY byte parsing.
use crate::detect::Agent;
use crate::events::AppEvent;
use crate::layout::PaneId;
use tokio::sync::mpsc;

#[derive(Default)]
pub(super) struct SessionIdentity {
    last: Option<String>,
}

impl SessionIdentity {
    pub(super) async fn observe(
        &mut self,
        events: &mpsc::Sender<AppEvent>,
        pane_id: PaneId,
        agent: Option<Agent>,
        content: &str,
        process_exited: bool,
    ) {
        if agent != Some(Agent::Codex) || process_exited {
            self.last = None;
            return;
        }
        let Some(id) = codex_footer_id(content) else {
            return;
        };
        if self.last.as_deref() == Some(id) {
            return;
        }
        let event = AppEvent::CodexSessionObserved {
            pane_id,
            session_id: id.to_owned(),
        };
        if events.send(event).await.is_ok() {
            self.last = Some(id.to_owned());
        }
    }
}

fn codex_footer_id(content: &str) -> Option<&str> {
    // Codex places the configured status line immediately above its shortcut
    // hint. Bound the search to those footer rows, never conversation history.
    let mut rows = content
        .lines()
        .rev()
        .map(str::trim)
        .skip_while(|s| s.is_empty());
    let hint = rows.next()?;
    // A warning badge can share the right side of this row.
    if !(hint.starts_with("? for shortcuts") || hint.starts_with("← for agents · ? for shortcuts"))
    {
        return None;
    }
    let mut found = None;
    for row in rows.by_ref().take(8) {
        if row.is_empty() {
            // The blank separator prevents a multiline composer draft from
            // being mistaken for a status field. Require the nearby composer.
            return rows
                .take(8)
                .any(|line| line.starts_with('›'))
                .then_some(found)
                .flatten();
        }
        // Integration puts the ID first. Any row above it before the blank
        // separator means this was a different status field or a draft.
        if found.is_some() || row.starts_with('›') {
            return None;
        }
        let id = row.split(" · ").next()?.trim();
        if id.len() == 36
            && id.bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
        {
            if found.is_some() {
                return None;
            }
            found = Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

    #[test]
    fn session_footer_is_bounded_and_requires_a_complete_id() {
        for footer in [
            format!("› \n\n  {ID} · model · directory\n  ? for shortcuts"),
            format!("› \n\n  {ID}\n  model · directory\n  ← for agents · ? for shortcuts       ⚠ 1 warning · f2 to view"),
        ] {
            assert_eq!(codex_footer_id(&footer), Some(ID));
        }
        for screen in [
            format!("{ID}\n› \n  model\n  ? for shortcuts"),
            format!("› \n  {}\n  ? for shortcuts", &ID[..32]),
            format!("› \n  {ID}\n  {ID}\n  ? for shortcuts"),
            format!("{ID}\n  ? for shortcuts"),
            format!("› draft\n{ID}\n\n  model\n  ? for shortcuts"),
        ] {
            assert_eq!(codex_footer_id(&screen), None);
        }
    }

    #[tokio::test]
    async fn duplicate_observations_are_suppressed_until_process_exit() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut identity = SessionIdentity::default();
        let pane = PaneId::from_raw(1);
        let content = format!("› \n\n  {ID}\n  ? for shortcuts");
        identity
            .observe(&tx, pane, Some(Agent::Codex), &content, false)
            .await;
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::CodexSessionObserved { .. })
        ));
        identity
            .observe(&tx, pane, Some(Agent::Codex), &content, false)
            .await;
        assert!(rx.try_recv().is_err());
        identity
            .observe(&tx, pane, Some(Agent::Codex), &content, true)
            .await;
        assert!(rx.try_recv().is_err());
        identity
            .observe(&tx, pane, Some(Agent::Codex), &content, false)
            .await;
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::CodexSessionObserved { .. })
        ));
    }
}
