//! Per-dispatch execution identities and replayable close operations.
use super::binding::{save, unknown, valid_launch_id, Claim};
use super::{failure, Reply, Tmux};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::ErrorKind,
};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CloseEffect {
    AlreadyAbsent,
    SessionClosed,
    WorkerExited,
    UnstartedSessionClosed,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) enum CloseOperation {
    Pending,
    NotExecuted,
    Confirmed(CloseEffect),
}

// Mutations run under the dispatch lock. The public attempt and a close
// operation have independent lifetimes from the current session pointer.
#[derive(Default, Deserialize, Serialize)]
struct Registry {
    launches: BTreeMap<String, String>,
    #[serde(default)]
    legacy_launches: BTreeMap<String, String>,
    closed: BTreeSet<String>,
    close_operations: BTreeMap<String, BTreeMap<String, CloseOperation>>,
}

impl Tmux<'_> {
    fn saved_registry(&self) -> Reply<Option<Registry>> {
        match File::open(self.claim_path().with_extension("executions.json")) {
            Ok(f) => serde_json::from_reader(f).map(Some).map_err(|_| unknown()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(_) => Err(unknown()),
        }
    }

    fn registry(&self) -> Reply<Registry> {
        Ok(self.saved_registry()?.unwrap_or_default())
    }

    fn save_registry(&self, registry: &Registry) -> Reply<()> {
        save(
            &self.claim_path().with_extension("executions.json"),
            registry,
        )
        .map_err(|_| unknown())
    }

    pub(super) fn was_released(&self, id: &str) -> Reply<bool> {
        Ok(
            matches!(self.claim()?, Some(Claim::Released(Some(old))) if old == id)
                || self.registry()?.closed.contains(id),
        )
    }

    pub(super) fn registered_launch(&self, execution: &str) -> Reply<String> {
        if let Some(Claim::Registered {
            id,
            execution: active,
        }) = self.claim()?
        {
            if active == execution && valid_launch_id(&id) {
                return Ok(id);
            }
        }
        let registry = self.registry()?;
        registry
            .launches
            .get(execution)
            .filter(|id| valid_launch_id(id) && registry.closed.contains(*id))
            .cloned()
            .ok_or_else(unknown)
    }

    // Called under the same dispatch lock as reserve, replacement and close.
    // A missing lookup alone cannot distinguish an unregistered attempt from
    // damaged state. Require a surviving, keyed predecessor as the anchor.
    pub(super) fn resume_registration(&self, execution: &str, predecessor: &str) -> Reply<Value> {
        let claim = self.claim()?;
        let registry = self.saved_registry()?.ok_or_else(unknown)?;
        let current = match &claim {
            Some(Claim::Registered { id, execution }) => {
                if !valid_launch_id(id) {
                    return Err(unknown());
                }
                Some((execution.as_str(), id.as_str()))
            }
            Some(Claim::Released(Some(id)))
                if valid_launch_id(id) && registry.closed.contains(id) =>
            {
                registry
                    .launches
                    .get(predecessor)
                    .filter(|known| *known == id)
                    .map(|known| (predecessor, known.as_str()))
            }
            _ => None,
        };
        let mut evidence =
            json!({"result":"accepted","executionId":execution,"fromExecutionId":predecessor});
        let archived = registry.launches.get(execution);
        if archived.is_some_and(|id| !valid_launch_id(id) || !registry.closed.contains(id)) {
            return Err(unknown());
        }
        let active = current
            .filter(|(key, _)| *key == execution)
            .map(|(_, id)| id);
        if active
            .zip(archived)
            .is_some_and(|(active, old)| active != old.as_str())
        {
            return Err(unknown());
        }
        if let Some(id) = active.or_else(|| archived.map(String::as_str)) {
            self.submission(id, Some(execution))?;
            let not_started = self.cancellation(id, Some(execution))?.is_some()
                || super::launch::read_receipt(&self.launch_path(id, "json")).is_some_and(
                    |receipt| {
                        receipt["launchId"] == id
                            && receipt
                                .get("dispatch")
                                .is_none_or(|dispatch| dispatch.as_str() == Some(self.dispatch))
                            && receipt
                                .get("executionId")
                                .is_none_or(|key| key.as_str() == Some(execution))
                            && matches!(
                                receipt["category"].as_str(),
                                Some("executable_missing" | "permission_denied" | "spawn_failed")
                            )
                    },
                );
            evidence["registration"] = json!(if not_started {
                "not_started"
            } else {
                "registered"
            });
            evidence["launchId"] = json!(id);
        } else if let Some((key, id)) = current.filter(|(key, _)| *key == predecessor) {
            self.submission(id, Some(key))?;
            evidence["registration"] = json!("unregistered");
        } else {
            return Err(unknown());
        }
        Ok(evidence)
    }

    pub(super) fn archived_execution(&self, launch_id: &str) -> Reply<Option<String>> {
        Ok(self
            .registry()?
            .launches
            .into_iter()
            .find_map(|(execution, launch)| (launch == launch_id).then_some(execution)))
    }

    pub(super) fn observed_launch(
        &self,
        execution: &str,
        terminal_only: bool,
    ) -> Reply<Option<String>> {
        let claim = self.claim()?;
        let launch = match &claim {
            Some(observed @ Claim::Observed { receipt, .. })
                if observed.execution_id() == Some(execution) =>
            {
                receipt.clone()
            }
            _ => self.registry()?.legacy_launches.get(execution).cloned(),
        };
        Ok(launch.filter(|id| {
            valid_launch_id(id)
                && self.legacy_launch_id.is_none_or(|hint| hint == id)
                && self
                    .receipt_worker(id)
                    .is_ok_and(|worker| !terminal_only || worker == super::binding::Worker::Exited)
        }))
    }

    pub(super) fn previous_close(
        &self,
        execution: &str,
        operation: &str,
    ) -> Reply<Option<CloseOperation>> {
        Ok(self
            .registry()?
            .close_operations
            .get(execution)
            .and_then(|ops| ops.get(operation))
            .copied())
    }

    pub(super) fn record_close(&self, effect: CloseOperation) -> Reply<()> {
        if let (Some(id), Some(operation)) = (self.legacy_execution_id, self.close_operation_id) {
            let mut registry = self.registry()?;
            registry
                .close_operations
                .entry(id.to_owned())
                .or_default()
                .insert(operation.to_owned(), effect);
            self.save_registry(&registry)?;
        }
        Ok(())
    }

    // Archive the outgoing generation before changing the current pointer.
    // This also imports the last Released record written by earlier versions.
    pub(super) fn replace_claim(&self, next: &Claim) -> Reply<()> {
        if let Some(previous) = self.claim()? {
            if previous.execution_id() != next.execution_id() || matches!(next, Claim::Released(_))
            {
                self.archive(&previous)?;
            }
        }
        save(&self.claim_path(), next).map_err(|_| unknown())
    }

    fn archive(&self, claim: &Claim) -> Reply<()> {
        if matches!(claim, Claim::LegacyPending { .. }) {
            return Ok(());
        }
        let Some(id) = claim.execution_id() else {
            return Ok(());
        };
        let mut registry = self.registry()?;
        let before = registry.closed.len();
        registry.closed.insert(id.to_owned());
        if let Claim::Observed { token, receipt, .. } = claim {
            registry.closed.insert(token.clone());
            registry.closed.extend(receipt.iter().cloned());
            if let Some(launch) = receipt {
                registry
                    .legacy_launches
                    .insert(id.to_owned(), launch.clone());
                // Persist the association even when a prior release already archived the IDs.
                return self.save_registry(&registry);
            }
        }
        let archived_assignment = if let Claim::Registered { id, execution } = claim {
            registry
                .launches
                .insert(execution.clone(), id.clone())
                .as_ref()
                != Some(id)
        } else {
            false
        };
        if archived_assignment || registry.closed.len() != before {
            self.save_registry(&registry)?;
        }
        Ok(())
    }

    pub(super) fn check_new_execution(&self) -> Reply<()> {
        let Some(execution) = self.new_execution_id else {
            return Ok(());
        };
        if let Some(Claim::Registered {
            id,
            execution: active,
        }) = self.claim()?
        {
            if active == execution {
                // Retry only a proven unstarted reservation before any keyed close.
                if (self.legacy_worker_cancelled(&id)
                    || self.cancellation(&id, Some(execution))?.is_some())
                    && !self.was_released(&id)?
                    && !self.registry()?.close_operations.contains_key(&id)
                {
                    return Ok(());
                }
                return Err(failure(
                    "refused",
                    "execution already has a launch reservation",
                ));
            }
        }
        if self.registry()?.launches.contains_key(execution) {
            return Err(failure("refused", "execution has already ended"));
        }
        Ok(())
    }

    pub(super) fn reserve(&self, id: &str) -> Reply<()> {
        // Publish both identifiers in one rename, before starting tmux.
        let claim = match self.new_execution_id {
            Some(execution) => Claim::Registered {
                id: id.to_owned(),
                execution: execution.to_owned(),
            },
            None => Claim::Launch(id.to_owned()),
        };
        self.replace_claim(&claim)?;
        if self.new_execution_id.is_some() && self.saved_registry()?.is_none() {
            self.save_registry(&Registry::default())?;
        }
        // Persist the registration before launching the worker. A later
        // reconciliation may use the surviving predecessor as absence proof.
        File::open(self.state.join("tmux"))
            .and_then(|dir| dir.sync_all())
            .map_err(|_| unknown())?;
        File::open(self.state)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| unknown())
    }
}
