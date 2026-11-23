#![cfg(target_os = "linux")]

//! Linux adapter for an external Herdr 0.9.5 server. The dispatch owner holds
//! its per-attempt lock across each operation. Checkpoints must be durable on
//! return. Reconcile produces a proposed binding and audit for one atomic commit.
mod process;
mod rpc;
mod screen;
pub use process::Process;
use rpc::Session;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    DefinitelyNotSent,
    Refused,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Failure {
    #[serde(rename = "result")]
    pub kind: FailureKind,
    pub diagnostic: &'static str,
}
impl Failure {
    fn refused(diagnostic: &'static str) -> Self {
        Self {
            kind: FailureKind::Refused,
            diagnostic,
        }
    }
    fn unsent(diagnostic: &'static str) -> Self {
        Self {
            kind: FailureKind::DefinitelyNotSent,
            diagnostic,
        }
    }
    const fn unknown(diagnostic: &'static str) -> Self {
        Self {
            kind: FailureKind::Uncertain,
            diagnostic,
        }
    }
    fn after_effect(self) -> Self {
        Self::unknown(self.diagnostic)
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.diagnostic)
    }
}
impl std::error::Error for Failure {}

#[derive(Debug, Deserialize)]
pub struct Launch {
    pub cwd: PathBuf,
    pub label: String,
    /// Prepared worker argv, typically the common launcher's private spec path.
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handle {
    pub endpoint: PathBuf,
    pub attempt: String,
    pub nonce: String,
    pub service: Option<Process>,
    pub workspace: Option<String>,
    pub pane: Option<String>,
    pub terminal: Option<String>,
    #[serde(rename = "writerProcesses")]
    pub writers: Vec<Process>,
    #[serde(
        default,
        rename = "writerStoppedEvidence",
        skip_serializing_if = "Option::is_none"
    )]
    pub stopped_evidence: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Mapping {
    pub workspace: Option<String>,
    pub pane: String,
    pub terminal: String,
    pub actor: String,
    pub evidence: String,
    pub old_writer_evidence: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Binding {
    pub handle: Handle,
    pub audit: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Screen {
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}

const INVALID: Failure = Failure::unknown("incomplete Herdr observation");

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().filter(|s| !s.is_empty()).ok_or(INVALID)
}

fn record<'a>(snapshot: &'a Value, list: &str, key: &str, id: &str) -> Result<Option<&'a Value>> {
    let mut found = None;
    for row in snapshot[list].as_array().ok_or(INVALID)? {
        if field(row, key)? == id && found.replace(row).is_some() {
            return Err(INVALID);
        }
    }
    Ok(found)
}

impl Handle {
    /// Call once for a reserved create. Failure after workspace creation keeps
    /// the last checkpoint pending. Only observation may be retried.
    pub fn create(
        endpoint: &Path,
        attempt: &str,
        launch: &Launch,
        mut save: impl FnMut(&Self) -> std::io::Result<()>,
    ) -> Result<Self> {
        if attempt.trim().is_empty()
            || !launch.cwd.is_absolute()
            || launch.cwd.to_str().is_none()
            || !launch.cwd.is_dir()
            || launch.command.first().is_none_or(|s| s.is_empty())
            || launch.command.iter().any(|arg| arg.contains('\0'))
        {
            return Err(Failure::refused("invalid Herdr launch"));
        }
        let mut checkpoint = |handle: &Self| {
            save(handle).map_err(|_| Failure::unknown("Herdr checkpoint could not be committed"))
        };
        let mut handle = Self {
            endpoint: endpoint.into(),
            attempt: attempt.into(),
            nonce: Uuid::new_v4().simple().to_string(),
            service: None,
            workspace: None,
            pane: None,
            terminal: None,
            writers: vec![],
            stopped_evidence: None,
        };
        let rpc = Session::open(endpoint, None)?;
        handle.service = Some(rpc.service.clone());
        checkpoint(&handle)?;
        let created = rpc.call(
            "workspace.create",
            json!({"label":launch.label, "cwd":launch.cwd, "focus":false}),
        )?;
        let mut finish = || -> Result<()> {
            let workspace = field(&created["workspace"], "workspace_id")?;
            let tab = field(&created["tab"], "tab_id")?;
            handle.workspace = Some(workspace.to_owned());
            checkpoint(&handle)?;
            let snapshot = rpc.call("session.snapshot", json!({}))?;
            let snapshot = &snapshot["snapshot"];
            let fresh =
                record(snapshot, "workspaces", "workspace_id", workspace)?.ok_or(INVALID)?;
            let root = &created["root_pane"];
            let initial =
                record(snapshot, "panes", "pane_id", field(root, "pane_id")?)?.ok_or(INVALID)?;
            if fresh["tokens"].get("wfc_owner").is_some()
                || fresh["tokens"].get("wfc_attempt").is_some()
                || initial["workspace_id"] != workspace
                || initial["tab_id"] != tab
                || field(initial, "terminal_id")? != field(root, "terminal_id")?
            {
                return Err(Failure::unknown("initial Herdr workspace identity changed"));
            }
            rpc.call(
                "workspace.report_metadata",
                json!({"workspace_id":workspace,"source":"wfc",
                "tokens":{"wfc_owner":handle.nonce,"wfc_attempt":attempt}}),
            )?;
            let snapshot = rpc.call("session.snapshot", json!({}))?;
            let snapshot = &snapshot["snapshot"];
            handle.owned_workspace(snapshot)?.ok_or(INVALID)?;
            let initial = record(snapshot, "tabs", "tab_id", tab)?.ok_or(INVALID)?;
            if initial["workspace_id"] != workspace || initial["pane_count"] != 1 {
                return Err(INVALID);
            }
            let layout = rpc.call(
                "layout.apply",
                json!({"tab_id":tab,"focus":false,"root":{
                "type":"pane","label":launch.label,"cwd":launch.cwd,"command":launch.command}}),
            )?;
            let pane = field(&layout["layout"], "focused_pane_id")?;
            handle.pane = Some(pane.into());
            checkpoint(&handle)?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let snapshot = rpc.call("session.snapshot", json!({}))?;
                let snapshot = &snapshot["snapshot"];
                handle.owned_workspace(snapshot)?.ok_or(INVALID)?;
                if let Some(row) = record(snapshot, "panes", "pane_id", pane)? {
                    if row["workspace_id"] != workspace {
                        return Err(INVALID);
                    }
                    handle.terminal = Some(field(row, "terminal_id")?.into());
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(INVALID);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            checkpoint(&handle)?;
            handle.writers = handle.observe_writers(&rpc)?;
            checkpoint(&handle)
        };
        finish().map_err(Failure::after_effect)?;
        Ok(handle)
    }

    fn session(&self) -> Result<Session<'_>> {
        let service = self.service.as_ref().ok_or(INVALID)?;
        Session::open(&self.endpoint, Some(service))
    }

    fn owned_workspace<'a>(&self, snapshot: &'a Value) -> Result<Option<&'a Value>> {
        let id = self
            .workspace
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or(INVALID)?;
        if self.nonce.trim().is_empty() || self.attempt.trim().is_empty() {
            return Err(INVALID);
        }
        let workspace = record(snapshot, "workspaces", "workspace_id", id)?;
        if workspace.is_some_and(|w| {
            w["tokens"]["wfc_owner"] != self.nonce || w["tokens"]["wfc_attempt"] != self.attempt
        }) {
            return Err(Failure::refused("Herdr workspace ownership mismatch"));
        }
        Ok(workspace)
    }

    fn pane<'a>(&self, snapshot: &'a Value) -> Result<Option<&'a Value>> {
        let workspace = self.owned_workspace(snapshot)?;
        let pane = self
            .pane
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(INVALID)?;
        let terminal = self
            .terminal
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(INVALID)?;
        let row = record(snapshot, "panes", "pane_id", pane)?;
        if let Some(row) = row {
            if workspace.is_none()
                || row["workspace_id"].as_str() != self.workspace.as_deref()
                || row["terminal_id"] != terminal
            {
                return Err(Failure::refused("Herdr pane or terminal mismatch"));
            }
        }
        Ok(row)
    }

    fn guarded(&self, rpc: &Session, input: bool) -> Result<()> {
        let snapshot = rpc.call("session.snapshot", json!({}))?;
        let pane = self
            .pane(&snapshot["snapshot"])?
            .ok_or(Failure::unsent("Herdr pane absent"))?;
        if input && field(pane, "agent_status")? == "blocked" {
            return Err(Failure::refused("Herdr reports a blocked worker"));
        }
        Ok(())
    }

    fn observe_writers(&self, rpc: &Session) -> Result<Vec<Process>> {
        self.guarded(rpc, false)?;
        let pane = self.pane.as_deref().ok_or(INVALID)?;
        let data = rpc.call("pane.process_info", json!({"pane_id":pane}))?;
        process::writers(&data["process_info"], pane)
    }

    fn bound_writers(&self, rpc: &Session) -> Result<Vec<Process>> {
        let writers = self.observe_writers(rpc)?;
        if self.stopped_evidence.is_some()
            || self.writers.first().ok_or(INVALID)? != writers.first().ok_or(INVALID)?
        {
            return Err(Failure::unknown("Herdr pane writer identity changed"));
        }
        Ok(writers)
    }

    fn writers_alive(&self) -> Result<bool> {
        if self.writers.is_empty() && self.stopped_evidence.is_none() {
            return Err(INVALID);
        }
        let mut live = false;
        for writer in &self.writers {
            live |= writer.alive()?;
        }
        Ok(live)
    }

    /// Returns false only when every recorded writer has positively stopped.
    pub fn inspect(&self) -> Result<bool> {
        let rpc = self.session()?;
        let snap = rpc.call("session.snapshot", json!({}))?;
        let present = self.pane(&snap["snapshot"])?.is_some();
        if present {
            self.bound_writers(&rpc)?;
        }
        let live = self.writers_alive()?;
        if live && !present {
            return Err(Failure::unknown("writer survives missing Herdr pane"));
        }
        Ok(live)
    }

    pub fn read(&self) -> Result<Screen> {
        let rpc = self.session()?;
        self.guarded(&rpc, false)?;
        let data = rpc.call("pane.read",json!({"pane_id":self.pane,"source":"recent","format":"text","strip_ansi":true,"lines":200}))?;
        if data["read"]["pane_id"].as_str() != self.pane.as_deref()
            || data["read"]["workspace_id"].as_str() != self.workspace.as_deref()
        {
            return Err(INVALID);
        }
        serde_json::from_value(data["read"].clone()).map_err(|_| INVALID)
    }

    fn send(&self, method: &str, params: Value, input: bool) -> Result<()> {
        let rpc = self.session()?;
        self.bound_writers(&rpc)?;
        self.guarded(&rpc, input)?;
        rpc.call(method, params)?;
        Ok(())
    }

    pub fn interrupt(&self) -> Result<()> {
        self.send(
            "pane.send_keys",
            json!({"pane_id":self.pane,"keys":["C-c"]}),
            false,
        )
    }

    /// Caller holds the dispatch input claim. On uncertain, retain the claim.
    pub fn submit(&self, text: &str, codex: bool) -> Result<()> {
        let before = self.read()?;
        if screen::read_only(&before.text) {
            return Err(Failure::refused("worker input is paused"));
        }
        self.send(
            "pane.send_input",
            json!({"pane_id":self.pane,"text":text,"keys":["Enter"]}),
            true,
        )?;
        if codex {
            screen::confirm_codex(self, text, &before).map_err(Failure::after_effect)?;
        }
        Ok(())
    }

    /// Close only the bound pane. Persist all observed writers before closing,
    /// so a lost response cannot erase the evidence needed by a later close.
    pub fn close(&mut self, mut save: impl FnMut(&Self) -> std::io::Result<()>) -> Result<()> {
        let endpoint = self.endpoint.clone();
        let rpc = Session::open(&endpoint, Some(self.service.as_ref().ok_or(INVALID)?))?;
        let snap = rpc.call("session.snapshot", json!({}))?;
        if self.pane(&snap["snapshot"])?.is_some() {
            let current = self.bound_writers(&rpc)?;
            for writer in current {
                if !self.writers.contains(&writer) {
                    self.writers.push(writer);
                }
            }
            save(self).map_err(|_| Failure::unknown("cannot checkpoint writers before close"))?;
            self.guarded(&rpc, false)?;
            rpc.call("pane.close", json!({"pane_id":self.pane}))?;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !self.writers_alive()? {
                let snap = rpc
                    .call("session.snapshot", json!({}))
                    .map_err(Failure::after_effect)?;
                if self
                    .pane(&snap["snapshot"])
                    .map_err(Failure::after_effect)?
                    .is_none()
                {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(Failure::unknown("Herdr pane or old writer still present"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Observe an explicit proposed mapping. Caller verifies current owner and
    /// attempt again when persisting the returned handle and audit together.
    pub fn reconcile(&self, mapping: &Mapping) -> Result<Binding> {
        if [
            &mapping.actor,
            &mapping.evidence,
            &mapping.pane,
            &mapping.terminal,
        ]
        .iter()
        .any(|v| v.trim().is_empty())
        {
            return Err(Failure::refused(
                "mapping requires pane, terminal, actor and evidence",
            ));
        }
        let rpc = Session::open(&self.endpoint, None)?;
        let mut next = self.clone();
        next.workspace = mapping.workspace.clone().or_else(|| self.workspace.clone());
        next.pane = Some(mapping.pane.clone());
        next.terminal = Some(mapping.terminal.clone());
        next.service = Some(rpc.service.clone());
        let same_mapping = self.pane == next.pane
            && self.terminal == next.terminal
            && self.workspace == next.workspace;
        let stop_proof = mapping
            .old_writer_evidence
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        if (self.service != next.service || !same_mapping) && stop_proof.is_none() {
            return Err(Failure::refused(
                "changed binding requires old writer evidence",
            ));
        }
        let snapshot = rpc.call("session.snapshot", json!({}))?;
        if next.pane(&snapshot["snapshot"])?.is_some() {
            next.writers = next.observe_writers(&rpc)?;
            if self.writers.first() != next.writers.first() && stop_proof.is_none() {
                return Err(Failure::refused(
                    "changed writer requires old writer evidence",
                ));
            }
            next.stopped_evidence = None;
        } else {
            let evidence = stop_proof.ok_or(Failure::refused(
                "absent pane requires old writer stop evidence",
            ))?;
            if !same_mapping {
                return Err(Failure::refused(
                    "missing pane does not verify proposed mapping",
                ));
            }
            next.writers.clear();
            next.stopped_evidence = Some(evidence.into());
        }
        for old in &self.writers {
            if !next.writers.contains(old) && old.alive()? {
                return Err(Failure::unknown(
                    "old writer still alive outside proposed binding",
                ));
            }
        }
        let audit = json!({
            "operation": "herdr-reconcile",
            "actor": mapping.actor,
            "evidence": mapping.evidence,
            "oldWriterEvidence": mapping.old_writer_evidence,
            "state": if next.stopped_evidence.is_some() { "not-started" } else { "active" },
            "attempt": self.attempt,
            "previousService": self.service,
            "previousWriters": self.writers,
            "service": next.service,
            "writers": next.writers,
            "workspace": next.workspace,
            "pane": next.pane,
            "terminal": next.terminal,
            "inputReplayed": false,
        });
        Ok(Binding {
            handle: next,
            audit,
        })
    }
}
