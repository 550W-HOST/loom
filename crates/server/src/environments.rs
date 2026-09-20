//! Environment lifecycle: provisioning dispatch and worker reports.
//!
//! This is the control plane's half of [`EnvironmentProvision`]. It mirrors
//! [`crate::runs`] deliberately:
//!
//! 1. **Provisioning goes through the relay.** [`AppState::provision_environment`]
//!    moves a managed environment to `provisioning` and publishes an
//!    [`EnvironmentProvision`] to `host:{id}`. The handler never touches a
//!    worker socket, so a worker that is momentarily disconnected still gets the
//!    request on reconnect.
//! 2. **Reports become environment events.** [`AppState::apply_environment_report`]
//!    turns the worker's observation into the `environment_status_changed` event
//!    (and records the workspace path on success), published to the project
//!    scope where clients can replay it.
//!
//! A managed environment is the only kind that is provisioned; an unmanaged one
//! already names a directory and starts `ready`.

use loom_domain::{Environment, EnvironmentId, EnvironmentKind, EnvironmentStatus, HostId};
use loom_provider_protocol::{
    EnvironmentProvision, EnvironmentProvisionOutcome, EnvironmentProvisionReport,
};
use loom_relay::{now_ms, Scope};

use crate::state::AppState;

/// What happened when a provisioning dispatch was attempted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// The request is in the host's scope and the environment is `provisioning`.
    Dispatched(Environment),
    /// The environment is unknown to this server.
    Unknown,
    /// The environment cannot be provisioned from its current state.
    NotProvisionable {
        /// The environment, unchanged.
        environment: Environment,
        /// Why it was refused.
        reason: String,
    },
    /// The relay rejected the append; the environment was moved to `error`.
    PublishFailed {
        /// The environment, now in `error`.
        environment: Environment,
        /// Why the append failed.
        error: String,
    },
}

/// Whether a worker's provisioning report was applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentReportOutcome {
    /// The status event was published.
    Applied,
    /// The environment is not known to this server.
    Unknown,
    /// The report named an environment this host does not own.
    Mismatch(String),
    /// The environment was not awaiting provisioning: a duplicate or a report
    /// that arrived after the environment was already terminal.
    Stale,
}

impl AppState {
    /// Moves a managed environment to `provisioning` and publishes the request
    /// to its host's scope.
    ///
    /// A no-op transition (already `provisioning`) is rejected as
    /// `NotProvisionable` rather than published twice; a retry is
    /// `creating | error -> provisioning`, which the domain allows.
    pub fn provision_environment(&self, environment_id: &EnvironmentId) -> ProvisionOutcome {
        let now = now_ms();
        let Some(environment) = self.registry.environment(environment_id) else {
            return ProvisionOutcome::Unknown;
        };
        if environment.kind != EnvironmentKind::Managed {
            return ProvisionOutcome::NotProvisionable {
                environment,
                reason: "only a managed environment is provisioned".into(),
            };
        }
        if !matches!(
            environment.status,
            EnvironmentStatus::Creating | EnvironmentStatus::Error
        ) {
            return ProvisionOutcome::NotProvisionable {
                environment: environment.clone(),
                reason: format!(
                    "an environment in status {} cannot be provisioned",
                    environment.status
                ),
            };
        }

        let (environment, event) = match self.registry.set_environment_status(
            environment_id,
            EnvironmentStatus::Provisioning,
            now,
        ) {
            Ok(result) => result,
            Err(error) => {
                return ProvisionOutcome::NotProvisionable {
                    environment,
                    reason: error.to_string(),
                }
            }
        };
        let _ = self.publish_domain_event(&event);

        let provision = EnvironmentProvision {
            environment_id: environment.id.clone(),
            project_id: environment.project_id.clone(),
            host_id: environment.host_id.clone(),
            created_at_ms: now,
        };
        let payload =
            serde_json::to_vec(&provision).expect("an EnvironmentProvision always serializes");
        if let Err(error) = self.publish(Scope::Host(environment.host_id.to_string()), payload) {
            // Nothing reached the log, so no worker can report. Record the
            // failure rather than leaving the environment stuck provisioning.
            let environment = self
                .registry
                .fail_environment(&environment.id, error.to_string(), now)
                .map(|(environment, event)| {
                    let _ = self.publish_domain_event(&event);
                    environment
                })
                .unwrap_or(environment);
            return ProvisionOutcome::PublishFailed {
                environment,
                error: error.to_string(),
            };
        }
        ProvisionOutcome::Dispatched(environment)
    }

    /// Applies one worker provisioning report.
    ///
    /// A report for an environment this host does not own is rejected, so one
    /// machine cannot provision another's workspace. A report for an
    /// environment that is no longer `provisioning` is stale under redelivery
    /// and dropped, which keeps the worker's at-least-once delivery idempotent.
    pub fn apply_environment_report(
        &self,
        host_id: &HostId,
        report: EnvironmentProvisionReport,
    ) -> EnvironmentReportOutcome {
        let now = now_ms();
        let Some(environment) = self.registry.environment(&report.environment_id) else {
            return EnvironmentReportOutcome::Unknown;
        };
        if &environment.host_id != host_id {
            return EnvironmentReportOutcome::Mismatch(format!(
                "environment {} is owned by host {}, not {}",
                report.environment_id, environment.host_id, host_id
            ));
        }
        if environment.status != EnvironmentStatus::Provisioning {
            return EnvironmentReportOutcome::Stale;
        }

        let result = match report.outcome {
            EnvironmentProvisionOutcome::Provisioned { path } => self
                .registry
                .complete_environment_provisioning(&report.environment_id, path, now),
            EnvironmentProvisionOutcome::Failed { error } => {
                self.registry
                    .fail_environment(&report.environment_id, error, now)
            }
        };
        match result {
            Ok((_, event)) => {
                let _ = self.publish_domain_event(&event);
                EnvironmentReportOutcome::Applied
            }
            Err(crate::CommandError::NotFound(_)) => EnvironmentReportOutcome::Unknown,
            Err(crate::CommandError::Domain(
                loom_domain::DomainError::IllegalEnvironmentTransition { .. },
            )) => EnvironmentReportOutcome::Stale,
            Err(error) => EnvironmentReportOutcome::Mismatch(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppConfig;
    use loom_domain::HostId;
    use std::time::Duration;

    /// A state with reconciliation disabled; these tests never dispatch a run.
    fn state() -> AppState {
        AppState::build(AppConfig {
            reconcile_interval: Duration::ZERO,
            ..AppConfig::default()
        })
        .unwrap()
    }

    fn managed_environment(state: &AppState) -> (HostId, Environment) {
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let (environment, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host.id.clone(),
                EnvironmentKind::Managed,
                None,
                loom_relay::now_ms(),
            )
            .unwrap();
        (host.id, environment)
    }

    #[tokio::test]
    async fn provisioning_moves_a_managed_environment_and_dispatches() {
        let state = state();
        let (host_id, environment) = managed_environment(&state);

        let outcome = state.provision_environment(&environment.id);
        let ProvisionOutcome::Dispatched(provisioning) = outcome else {
            panic!("expected a dispatch, got {outcome:?}");
        };
        assert_eq!(provisioning.status, EnvironmentStatus::Provisioning);

        // The request is in the host room, so a reconnecting worker replays it.
        let scope = Scope::Host(host_id.to_string());
        let frames = state.relay.replay_scope(&scope, 10).unwrap();
        assert_eq!(frames.len(), 1);
        let frame: serde_json::Value = serde_json::from_slice(&frames[0].payload).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();
        assert_eq!(payload["environment_id"], environment.id.to_string());

        // The status change is in the project scope, where clients replay it.
        // (The creation event is not published here because the helper created
        // the environment through the registry directly.)
        let project = Scope::Project(environment.project_id.to_string());
        let frames = state.relay.replay_scope(&project, 10).unwrap();
        assert_eq!(frames.len(), 1);
        let frame: serde_json::Value = serde_json::from_slice(&frames[0].payload).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();
        assert_eq!(payload["type"], "environment_status_changed");
        assert_eq!(payload["to"], "provisioning");

        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_successful_report_records_the_path_and_reaches_ready() {
        let state = state();
        let (host_id, environment) = managed_environment(&state);
        state.provision_environment(&environment.id);

        let outcome = state.apply_environment_report(
            &host_id,
            EnvironmentProvisionReport {
                host_id: host_id.clone(),
                environment_id: environment.id.clone(),
                outcome: EnvironmentProvisionOutcome::Provisioned {
                    path: "/srv/work".into(),
                },
            },
        );
        assert_eq!(outcome, EnvironmentReportOutcome::Applied);

        let stored = state.registry.environment(&environment.id).unwrap();
        assert_eq!(stored.status, EnvironmentStatus::Ready);
        assert_eq!(stored.path.as_deref(), Some("/srv/work"));
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_failed_report_moves_the_environment_to_error_with_its_reason() {
        let state = state();
        let (host_id, environment) = managed_environment(&state);
        state.provision_environment(&environment.id);

        state.apply_environment_report(
            &host_id,
            EnvironmentProvisionReport {
                host_id: host_id.clone(),
                environment_id: environment.id.clone(),
                outcome: EnvironmentProvisionOutcome::Failed {
                    error: "permission denied".into(),
                },
            },
        );

        let stored = state.registry.environment(&environment.id).unwrap();
        assert_eq!(stored.status, EnvironmentStatus::Error);
        assert_eq!(stored.error.as_deref(), Some("permission denied"));

        // A retry is legal and starts clean.
        state.provision_environment(&environment.id);
        assert_eq!(
            state.registry.environment(&environment.id).unwrap().status,
            EnvironmentStatus::Provisioning
        );
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_report_from_the_wrong_host_is_rejected() {
        let state = state();
        let (_, environment) = managed_environment(&state);
        state.provision_environment(&environment.id);

        let outcome = state.apply_environment_report(
            &HostId::mint(),
            EnvironmentProvisionReport {
                host_id: HostId::mint(),
                environment_id: environment.id.clone(),
                outcome: EnvironmentProvisionOutcome::Provisioned {
                    path: "/srv/work".into(),
                },
            },
        );
        assert!(matches!(outcome, EnvironmentReportOutcome::Mismatch(_)));
        assert_eq!(
            state.registry.environment(&environment.id).unwrap().status,
            EnvironmentStatus::Provisioning
        );
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn a_stale_report_is_dropped() {
        let state = state();
        let (host_id, environment) = managed_environment(&state);
        // Never provisioned: still `creating`.
        let outcome = state.apply_environment_report(
            &host_id,
            EnvironmentProvisionReport {
                host_id: host_id.clone(),
                environment_id: environment.id.clone(),
                outcome: EnvironmentProvisionOutcome::Failed {
                    error: "late".into(),
                },
            },
        );
        assert_eq!(outcome, EnvironmentReportOutcome::Stale);
        assert_eq!(
            state.registry.environment(&environment.id).unwrap().status,
            EnvironmentStatus::Creating
        );
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn an_unmanaged_environment_is_never_provisioned() {
        let state = state();
        let (host, _) = state
            .registry
            .enroll_host(None, "laptop".into(), loom_relay::now_ms())
            .unwrap();
        let (environment, _) = state
            .registry
            .create_environment(
                Some(state.registry.personal_project_id()),
                host.id,
                EnvironmentKind::Unmanaged,
                Some("/srv/loom".into()),
                loom_relay::now_ms(),
            )
            .unwrap();

        let outcome = state.provision_environment(&environment.id);
        assert!(matches!(outcome, ProvisionOutcome::NotProvisionable { .. }));
        assert_eq!(
            state.registry.environment(&environment.id).unwrap().status,
            EnvironmentStatus::Ready
        );
        state.shutdown().unwrap();
    }

    #[tokio::test]
    async fn provisioning_an_unknown_environment_is_reported() {
        let state = AppState::build(AppConfig {
            reconcile_interval: Duration::from_millis(0),
            ..AppConfig::default()
        })
        .unwrap();
        assert_eq!(
            state.provision_environment(&EnvironmentId::mint()),
            ProvisionOutcome::Unknown
        );
        state.shutdown().unwrap();
    }
}
