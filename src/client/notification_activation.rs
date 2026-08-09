#[cfg(any(target_os = "macos", test))]
use std::collections::BTreeMap;
#[cfg(any(target_os = "macos", test))]
use std::time::{Duration, Instant};

#[cfg(any(not(windows), test))]
use super::shell::ClientNotificationTarget;
#[cfg(target_os = "macos")]
use super::shell::ClientShellInput;
use super::shell::ClientShellState;

#[cfg(any(target_os = "macos", test))]
const MAX_NOTIFICATION_TARGETS: usize = 256;
#[cfg(any(target_os = "macos", test))]
const NOTIFICATION_TARGET_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
#[cfg(target_os = "macos")]
const ACTIVATION_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(any(target_os = "macos", test))]
struct RegisteredTarget {
    target: ClientNotificationTarget,
    expires_at: Instant,
}

/// Native callbacks carry only a bounded, one-use local token. The corresponding
/// machine, server boot and pane remain in this client's memory.
#[derive(Default)]
pub(super) struct NotificationActivations {
    #[cfg(any(target_os = "macos", test))]
    next_id: u64,
    #[cfg(any(target_os = "macos", test))]
    targets: BTreeMap<u64, RegisteredTarget>,
    #[cfg(target_os = "macos")]
    pending: Option<RegisteredTarget>,
    #[cfg(target_os = "macos")]
    endpoint: Option<crate::platform::NotificationActivationEndpoint>,
}

impl NotificationActivations {
    #[cfg(target_os = "macos")]
    pub(super) fn start(&mut self, event_tx: tokio::sync::mpsc::Sender<super::ClientLoopEvent>) {
        match crate::platform::NotificationActivationEndpoint::start(move |activation_id| {
            // Drop joins this listener thread, so it must never wait for the
            // client loop to drain its bounded event queue during shutdown.
            if let Err(err) =
                event_tx.try_send(super::ClientLoopEvent::NotificationActivated(activation_id))
            {
                tracing::warn!(%err, "could not queue notification activation");
            }
        }) {
            Ok(endpoint) => self.endpoint = Some(endpoint),
            Err(err) => tracing::warn!(%err, "failed to start notification activation endpoint"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn start(&mut self, _event_tx: tokio::sync::mpsc::Sender<super::ClientLoopEvent>) {}

    #[cfg(any(target_os = "macos", test))]
    fn register(&mut self, target: ClientNotificationTarget, now: Instant) -> u64 {
        self.targets.retain(|_, target| now < target.expires_at);
        while self.targets.len() >= MAX_NOTIFICATION_TARGETS {
            self.targets.pop_first();
        }
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        self.targets.insert(
            self.next_id,
            RegisteredTarget {
                target,
                expires_at: now + NOTIFICATION_TARGET_LIFETIME,
            },
        );
        self.next_id
    }

    #[cfg(target_os = "macos")]
    pub(super) fn show(
        &mut self,
        title: &str,
        body: Option<&str>,
        target: Option<ClientNotificationTarget>,
    ) -> std::io::Result<bool> {
        let Some(target) = target else {
            return crate::terminal_notify::show_system_notification(title, body);
        };
        if self.endpoint.is_some() {
            let activation_id = self.register(target, Instant::now());
            let callback = self
                .endpoint
                .as_ref()
                .map(|endpoint| endpoint.callback_command(activation_id));
            if let Some(callback) = callback {
                match crate::platform::show_actionable_desktop_notification(title, body, &callback)
                {
                    Ok(true) => return Ok(true),
                    Ok(false) => {}
                    Err(err) => tracing::warn!(%err, "actionable desktop notification unavailable"),
                }
            }
            self.targets.remove(&activation_id);
        }
        // Pane notifications must never use the generic AppleScript fallback,
        // whose click opens Script Editor instead of the requested terminal.
        crate::terminal_notify::show_notification(title, body)
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub(super) fn show(
        &mut self,
        title: &str,
        body: Option<&str>,
        _target: Option<ClientNotificationTarget>,
    ) -> std::io::Result<bool> {
        crate::terminal_notify::show_system_notification(title, body)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn activate(
        &mut self,
        activation_id: u64,
        shell: &mut ClientShellState,
        now: Instant,
    ) -> Option<ClientShellInput> {
        let registered = self.targets.remove(&activation_id)?;
        if now >= registered.expires_at {
            return None;
        }
        let outcome = shell.focus_notification_target(&registered.target)?;
        self.pending = Some(RegisteredTarget {
            target: registered.target,
            expires_at: now + ACTIVATION_RENDER_TIMEOUT,
        });
        Some(outcome)
    }

    #[cfg(target_os = "macos")]
    fn take_presented_target(&mut self, shell: &ClientShellState, now: Instant) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if now >= pending.expires_at || !shell.notification_target_exists(&pending.target) {
            self.pending = None;
            return false;
        }
        if !shell.notification_target_is_presented(&pending.target) {
            return false;
        }
        self.pending = None;
        true
    }

    #[cfg(target_os = "macos")]
    pub(super) fn frame_presented(&mut self, shell: &ClientShellState) {
        if !self.take_presented_target(shell, Instant::now()) {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Err(err) = crate::platform::activate_host_terminal() {
            tracing::warn!(%err, "failed to activate host terminal after notification click");
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn frame_presented(&mut self, _shell: &ClientShellState) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ClientNotificationTarget {
        ClientNotificationTarget {
            endpoint_id: super::super::endpoint::ClientEndpointId::Local,
            boot_id: "boot".into(),
            pane_id: "pane".into(),
        }
    }

    #[test]
    fn native_notification_registry_is_bounded_and_expires_old_targets() {
        let mut registry = NotificationActivations::default();
        let now = Instant::now();
        for _ in 0..MAX_NOTIFICATION_TARGETS + 3 {
            registry.register(target(), now);
        }
        assert_eq!(registry.targets.len(), MAX_NOTIFICATION_TARGETS);
        assert!(!registry.targets.contains_key(&1));
        registry.register(target(), now + NOTIFICATION_TARGET_LIFETIME);
        assert_eq!(registry.targets.len(), 1);
        assert_eq!(registry.targets.values().next().unwrap().target, target());
    }
}
