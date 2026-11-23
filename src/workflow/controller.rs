//! Scope leases and the fence shared by controller operations.
//!
//! Serialize lease changes and ledger claims under messages.lock. A scoped
//! relay holds its scope lock from claim through external I/O and settlement,
//! releasing messages.lock while delivery is in flight.
use super::{ensure_state_writable, host, messages, writable_state_dir, write_json};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fmt,
    fs::{self, File, TryLockError},
    io::ErrorKind,
    path::Path,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const USAGE: &str = "wfc owner {acquire|claim|renew|takeover} --scope S [--owner ID] [--token TOKEN] [--owner-token TOKEN] [--lease SECONDS]";
pub const ALIASES: &[&str] = &["acquire", "claim", "renew", "takeover"];
pub(super) const WORKER_CALLBACK: &str = "WFC_WORKER_CALLBACK";
pub(super) const CALLBACK_HASH: &str = "workerCallbackHash";

pub(crate) fn is_credential(key: &str) -> bool {
    ["WFC_CONTROLLER_TOKEN", "WFC_OWNER_TOKEN", WORKER_CALLBACK]
        .iter()
        .any(|name| key.eq_ignore_ascii_case(name))
}

pub(crate) fn strip_credentials(command: &mut std::process::Command) {
    for (key, _) in env::vars_os() {
        if is_credential(&key.to_string_lossy()) {
            command.env_remove(key);
        }
    }
}

#[derive(Debug)]
pub struct ControllerError(Value);

impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for ControllerError {}

pub(super) fn worker_callback() -> Option<OsString> {
    env::var_os(WORKER_CALLBACK).filter(|v| !v.is_empty())
}

pub(super) fn invalid_worker_callback(operation: &str) -> ControllerError {
    ControllerError(json!({"error":"invalid_worker_callback", "operation":operation}))
}

pub fn require_controller(operation: &str) -> Result<()> {
    if worker_callback().is_some() {
        return Err(ControllerError(json!({
            "error":"worker_callback_only", "operation":operation
        }))
        .into());
    }
    Ok(())
}

pub(super) fn token(explicit: Option<&str>) -> Option<String> {
    explicit
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            ["WFC_CONTROLLER_TOKEN", "WFC_OWNER_TOKEN"]
                .into_iter()
                .find_map(|name| env::var_os(name).filter(|token| !token.is_empty()))
                .and_then(|token| token.into_string().ok())
        })
}

fn now() -> f64 {
    Utc::now().timestamp_micros() as f64 / 1_000_000.0
}

fn lease_time(until: f64) -> Result<String> {
    // The public CLI uses local time with whole-second display precision.
    let time = DateTime::from_timestamp(until.floor() as i64, 0)
        .context("lease timestamp is out of range")?
        .with_timezone(&Local);
    let local = time
        .naive_utc()
        .checked_add_offset(*time.offset())
        .context("local lease timestamp is out of range")?;
    Ok(local.format("%Y-%m-%d %H:%M:%S").to_string())
}

#[derive(Serialize, Deserialize)]
struct Lease {
    owner: String,
    generation: u64,
    until: f64,
    fingerprint: String,
    retired: BTreeSet<String>,
}

impl Lease {
    fn public(&self, now: f64) -> Result<Value> {
        Ok(json!({"owner":self.owner, "generation":self.generation,
            "leaseUntil":lease_time(self.until)?, "active":self.until > now}))
    }

    fn accepts(&self, token: Option<&str>, now: f64) -> bool {
        self.until > now
            && token.is_some_and(|token| self.fingerprint == messages::body_hash(token))
    }
}

fn stale(
    scope: &str,
    operation: &str,
    current: Option<&Lease>,
    now: f64,
) -> Result<ControllerError> {
    let mut detail = match current {
        Some(lease) => lease.public(now)?,
        None => json!({"owner":null, "generation":null, "leaseUntil":null, "active":false}),
    };
    detail["error"] = json!("stale_controller_owner");
    detail["operation"] = json!(operation);
    detail["scope"] = json!(scope);
    Ok(ControllerError(detail))
}

#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct Owners(BTreeMap<String, Lease>);

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LegacyOwnerReplacement {
    token_sha256: String,
    lease_until: f64,
}

#[cfg(target_os = "linux")]
pub(super) fn stage_legacy_owners(
    state: &Path,
    rows: Vec<Value>,
    mut replacements: BTreeMap<String, LegacyOwnerReplacement>,
) -> Result<Vec<Value>> {
    use super::legacy_import::{sha256_text, text};

    let old_fingerprints: BTreeSet<_> = rows
        .iter()
        .map(|row| text(row, "token").map(messages::body_hash))
        .collect::<Result<_>>()?;
    let mut new_fingerprints = BTreeSet::new();
    let mut owners = Owners::default();
    let mut issued = Vec::new();
    for row in rows {
        let scope = text(&row, "scope")?;
        let owner = text(&row, "owner")?;
        ensure!(host::clean(scope) == scope, "unsupported legacy owner scope {scope}");
        let prior = row["generation"].as_u64().filter(|n| *n > 0).context("invalid legacy owner generation")?;
        let generation = prior.checked_add(1).context("legacy owner generation exhausted")?;
        let old_until = row["lease_until"].as_f64().context("invalid legacy owner lease")?;
        lease_time(old_until)?;
        let replacement = replacements.remove(scope).with_context(|| format!("missing owner replacement for {scope}"))?;
        let fingerprint = replacement.token_sha256;
        ensure!(
            sha256_text(&fingerprint) && !old_fingerprints.contains(&fingerprint)
                && new_fingerprints.insert(fingerprint.clone()),
            "invalid or reused replacement fingerprint for scope {scope}"
        );
        let until = replacement.lease_until;
        ensure!(until.is_finite() && until > now(), "replacement lease must still be active for scope {scope}");
        lease_time(until)?;
        owners.0.insert(scope.to_owned(), Lease {
            owner: owner.to_owned(), generation, until, fingerprint, retired: old_fingerprints.clone(),
        });
        issued.push(json!({"scope":scope,"owner":owner,"previousGeneration":prior,
            "previousLeaseUntil":old_until,"generation":generation,"leaseUntil":until}));
    }
    ensure!(replacements.is_empty(), "owner replacement names a scope absent from the offline snapshot");
    write_json(&state.join("controller-owners.json"), &json!(owners), None)?;
    Ok(issued)
}

impl Owners {
    // Mutations publish one complete snapshot by rename. Read-only callers
    // can preflight or display a lease without creating state or taking a lock.
    pub fn read(state: &Path) -> Result<Self> {
        match File::open(state.join("controller-owners.json")) {
            Ok(file) => serde_json::from_reader(std::io::BufReader::new(file))
                .context("read controller owners"),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("open controller owners"),
        }
    }

    pub fn public(&self, scope: &str) -> Result<Option<Value>> {
        self.0
            .get(scope)
            .map(|lease| lease.public(now()))
            .transpose()
    }

    /// Authorize the scope and report whether it is managed. Writers call this under their lock.
    pub fn require(&self, scope: &str, token: Option<&str>, operation: &str) -> Result<bool> {
        if let Some(lease) = self.0.get(scope) {
            let now = now();
            if !lease.accepts(token, now) {
                return Err(stale(scope, operation, Some(lease), now)?.into());
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub fn preflight(
        &self,
        scope: Option<&str>,
        token: Option<&str>,
        operation: &str,
    ) -> Result<()> {
        if let Some(scope) = scope {
            self.require(scope, token, operation)?;
        } else if !self.0.is_empty() {
            return Err(ControllerError(json!({
                "error":"controller_owner_required", "operation":operation
            }))
            .into());
        }
        Ok(())
    }
}

fn scope_lock_file(state: &Path, scope: &str) -> Result<File> {
    let mut options = messages::private_file();
    options.create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK,
    );
    // Share the existing #9 per-scope transport lock from claim to settlement.
    let lock = options.open(state.join(format!("relay-{}.lock", messages::body_hash(scope))))?;
    ensure!(lock.metadata()?.is_file(), "invalid scope lock");
    Ok(lock)
}

#[cfg(target_os = "linux")]
pub(super) fn try_scope_lock(state: &Path, scope: &str) -> Result<Option<File>> {
    ensure_state_writable(state)?;
    let lock = scope_lock_file(state, scope)?;
    try_lock(&lock).map(|locked| locked.then_some(lock))
}

fn try_lock(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e).context("lock controller scope"),
    }
}

pub(super) fn lock_scope(state: &Path, scope: &str) -> Result<File> {
    ensure_state_writable(state)?;
    fs::create_dir_all(state)?;
    let lock = scope_lock_file(state, scope)?;
    let start = Instant::now();
    while !try_lock(&lock)? {
        let remaining = Duration::from_secs(30).saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(ControllerError(json!({
                "error":"controller_owner_lock_timeout", "requiresDecision":true
            }))
            .into());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
    ensure_state_writable(state)?;
    Ok(lock)
}

pub struct Owner<'a> {
    action: &'a str,
    scope: String,
    owner: String,
    replacement: Option<&'a str>,
    credential: Option<&'a str>,
    lease: f64,
}

impl<'a> Owner<'a> {
    pub fn parse(action: &'a str, args: &'a [OsString]) -> Result<Self> {
        let [scope, owner, replacement, credential, lease] = messages::options(
            args,
            ["--scope", "--owner", "--token", "--owner-token", "--lease"],
            None,
        )?;
        let scope = host::clean(scope.context("missing --scope")?);
        ensure!(!scope.is_empty(), "missing --scope");
        let lease = lease
            .unwrap_or("60")
            .parse::<f64>()
            .context("invalid lease")?;
        ensure!(lease.is_finite(), "expected a finite lease");
        Ok(Self {
            action: if action == "claim" { "acquire" } else { action },
            scope,
            owner: owner.map(str::to_owned).unwrap_or_else(|| {
                env::var("WFC_CONTROLLER_ID").unwrap_or_else(|_| "controller".into())
            }),
            replacement: replacement.filter(|token| !token.is_empty()),
            credential,
            lease,
        })
    }

    pub fn run(self) -> Result<Value> {
        let state = writable_state_dir()?;
        let _scope_lock = lock_scope(&state, &self.scope)?;
        let _ledger_lock = messages::lock_for_publish(&state)?;
        let mut owners = Owners::read(&state)?;
        let now = now();
        let until = now + self.lease;
        let lease_until = lease_time(until)?;
        let current = owners.0.get_mut(&self.scope);
        let (generation, token) = if self.action == "renew" {
            let token = self
                .replacement
                .map(str::to_owned)
                .or_else(|| token(self.credential));
            let (lease, token) = match (current, token) {
                (Some(lease), Some(token)) if lease.accepts(Some(&token), now) => (lease, token),
                (lease, _) => {
                    return Err(stale(&self.scope, self.action, lease.as_deref(), now)?.into());
                }
            };
            lease.until = until;
            (lease.generation, token)
        } else {
            let token = self
                .replacement
                .map(str::to_owned)
                .unwrap_or_else(|| format!("ct-{}", Uuid::new_v4().simple()));
            let fingerprint = messages::body_hash(&token);
            let mut retired = BTreeSet::new();
            let generation = if let Some(lease) = current {
                if self.action == "acquire" && lease.until > now {
                    return Err(ControllerError(json!({
                        "error":"scope_owned", "scope":self.scope,
                        "owner":lease.owner, "generation":lease.generation
                    }))
                    .into());
                }
                if lease.fingerprint == fingerprint || lease.retired.contains(&fingerprint) {
                    return Err(ControllerError(json!({
                        "error":"owner_token_conflict", "operation":self.action,
                        "scope":self.scope, "generation":lease.generation
                    }))
                    .into());
                }
                retired = std::mem::take(&mut lease.retired);
                retired.insert(std::mem::take(&mut lease.fingerprint));
                lease
                    .generation
                    .checked_add(1)
                    .context("owner generation exhausted")?
            } else {
                1
            };
            owners.0.insert(
                self.scope.clone(),
                Lease {
                    owner: self.owner.clone(),
                    generation,
                    until,
                    fingerprint,
                    retired,
                },
            );
            (generation, token)
        };
        write_json(&state.join("controller-owners.json"), &json!(owners), None)?;
        messages::sync_directory(&state)?;
        Ok(
            json!({"scope":self.scope, "owner":self.owner, "token":token,
            "generation":generation, "leaseUntil":lease_until, "action":self.action}),
        )
    }
}
