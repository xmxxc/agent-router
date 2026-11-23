//! Per-dispatch execution identities and replayable close operations.
use super::binding::{save, unknown, valid_launch_id, Claim};
use super::{failure, launch, Reply, Tmux};
use serde::{Deserialize, Serialize};
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
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) enum CloseOperation {
    Pending,
    Confirmed(CloseEffect),
}

// Mutations run under the dispatch lock. The public attempt and a close
// operation have independent lifetimes from the current session pointer.
#[derive(Default, Deserialize, Serialize)]
struct Registry {
    launches: BTreeMap<String, String>,
    closed: BTreeSet<String>,
    close_operations: BTreeMap<String, BTreeMap<String, CloseOperation>>,
}

impl Tmux<'_> {
    fn registry(&self) -> Reply<Registry> {
        match File::open(self.claim_path().with_extension("executions.json")) {
            Ok(f) => serde_json::from_reader(f).map_err(|_| unknown()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Registry::default()),
            Err(_) => Err(unknown()),
        }
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
                if launch::cancelled(self.state, &id)
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
        self.replace_claim(&claim)
    }
}
