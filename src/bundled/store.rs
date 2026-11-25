use super::*;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};

type Hmac256 = Hmac<Sha256>;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn sign(token: &str, bytes: &[u8]) -> String {
    let mut mac = Hmac256::new_from_slice(token.as_bytes()).expect("HMAC accepts any key length");
    mac.update(bytes);
    format!("{:x}", mac.finalize().into_bytes())
}

pub fn verify(token: &str, bytes: &[u8], signature: &str) -> Result<()> {
    ensure!(
        signature.len() == 64 && signature.is_ascii(),
        "invalid signature"
    );
    let mut tag = [0; 32];
    for (i, b) in tag.iter_mut().enumerate() {
        *b = u8::from_str_radix(&signature[i * 2..i * 2 + 2], 16)?;
    }
    let mut mac = Hmac256::new_from_slice(token.as_bytes()).expect("HMAC accepts any key length");
    mac.update(bytes);
    mac.verify_slice(&tag)
        .map_err(|_| anyhow::anyhow!("invalid signature"))
}

pub fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    check_private_dir(path)
}

pub fn check_private_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "private directory required"
    );
    Ok(())
}

fn new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub fn publish(path: &Path, value: &Value) -> Result<()> {
    publish_with(path, value, |tmp, path| Ok(fs::rename(tmp, path)?))
}

fn publish_with(
    path: &Path,
    value: &Value,
    install: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<()> {
    publish_with_sync(path, value, install, sync_directory)
}

fn sync_directory(dir: &Path) -> Result<()> {
    Ok(File::open(dir)?.sync_all()?)
}

fn link_preparation(tmp: &Path, path: &Path) -> Result<()> {
    Ok(fs::hard_link(tmp, path)?)
}

fn publish_with_sync(
    path: &Path,
    value: &Value,
    install: impl FnOnce(&Path, &Path) -> Result<()>,
    sync_dir: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let tmp = path.with_extension(fresh());
    let mut file = new_file(&tmp)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, value)?;
        file.sync_all()?;
        install(&tmp, path)?;
        sync_dir(path.parent().context("missing parent")?)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub fn read(path: &Path) -> Result<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= wire::LIMIT as u64,
        "invalid state file"
    );
    Ok(serde_json::from_reader(file)?)
}

// An anonymous private file avoids argv/env secrets, pipe backpressure, and
// surviving launch specs when either side dies during exec.
pub fn stage_cmd(stage: &str, dir: &Path, spec: &Value, token: &str) -> Result<Command> {
    let path = dir.join(fresh());
    let mut input = new_file(&path)?;
    fs::remove_file(path)?;
    write_grant(&mut input, stage, spec, token)?;
    input.seek(SeekFrom::Start(0))?;
    let mut cmd = Command::new("/proc/self/exe");
    for name in std::env::vars_os().map(|(name, _)| name) {
        if name
            .as_encoded_bytes()
            .eq_ignore_ascii_case(b"WFC_CONTROLLER_TOKEN")
            || name
                .as_encoded_bytes()
                .eq_ignore_ascii_case(b"WFC_OWNER_TOKEN")
            || name
                .as_encoded_bytes()
                .eq_ignore_ascii_case(b"WFC_WORKER_CALLBACK")
        {
            cmd.env_remove(name);
        }
    }
    cmd.arg(stage)
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(cmd)
}

pub fn write_grant(mut input: impl Write, stage: &str, spec: &Value, token: &str) -> Result<()> {
    let grant = json!([stage, spec]);
    let signature = sign(token, &serde_json::to_vec(&grant)?);
    serde_json::to_writer(&mut input, &json!({"spec":spec,"signature":signature}))?;
    Ok(())
}

fn process(pid: u32) -> Result<(Value, bool)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // comm can contain spaces and ')' (including in the final position).
    let (_, fields) = stat.rsplit_once(") ").context("invalid process stat")?;
    let start: u64 = fields
        .split_whitespace()
        .nth(19)
        .context("missing start time")?
        .parse()?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok((
        json!({"pid":pid,"boot":boot.trim(),"start_ticks":start}),
        !fields.starts_with("Z ") && !fields.starts_with("X "),
    ))
}

pub fn identity(pid: u32) -> Result<Value> {
    Ok(process(pid)?.0)
}

pub fn process_state(expected: &Value) -> &'static str {
    let Some(pid) = expected["pid"].as_u64().and_then(|n| u32::try_from(n).ok()) else {
        return "unknown";
    };
    match process(pid) {
        Ok((id, true)) if id == *expected => "alive",
        Ok(_) => "exited",
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|err| err.kind() == io::ErrorKind::NotFound) =>
        {
            "exited"
        }
        Err(_) => "unknown",
    }
}

pub fn valid_name(name: &str) -> bool {
    (1..=120).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub struct Store {
    pub run: PathBuf,
    pub ticket: Value,
    pub token: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartupDecision {
    Ready,
    Aborted,
}

pub(super) enum Preparation {
    NotInstalled(anyhow::Error),
    Installed,
    Existing,
    Uncertain,
}

impl Store {
    pub fn open(unit: &Path, ticket: &Value) -> Result<Self> {
        let generation = field(ticket, "generation")?;
        ensure!(
            uuid::Uuid::parse_str(generation).is_ok(),
            "invalid generation"
        );
        let run = unit.join(generation);
        let key = read(&run.join("key.json"))?;
        let token = key.as_str().context("invalid key")?.to_owned();
        ensure!(
            ticket["capabilityHash"] == digest(token.as_bytes()),
            "capability mismatch"
        );
        Ok(Self {
            run,
            ticket: ticket.clone(),
            token,
        })
    }

    pub fn save_signed(&self, kind: &str, data: Value) -> Result<()> {
        ensure!(
            kind != "ready" || self.ticket["startupProtocol"] != 1,
            "managed ready requires a startup decision"
        );
        // Promote the starting binding by rename. A ready file only appears
        // once complete, and no weaker binding survives that transition.
        let slot = if kind == "ready" { "starting" } else { kind };
        self.publish_signed(slot, kind, data)?;
        if kind == "ready" {
            fs::rename(self.run.join("starting.json"), self.run.join("ready.json"))?;
            File::open(&self.run)?.sync_all()?;
        }
        Ok(())
    }

    fn publish_signed(&self, slot: &str, kind: &str, data: Value) -> Result<()> {
        let path = self.run.join(format!("{slot}.json"));
        publish(&path, &self.signed_envelope(kind, &data)?)
    }

    fn signed_envelope(&self, kind: &str, data: &Value) -> Result<Value> {
        let body = json!({"version":1,"kind":kind,"ticket":self.ticket,"data":data});
        let mac = sign(&self.token, &serde_json::to_vec(&body)?);
        Ok(json!({"body":body,"signature":mac}))
    }

    pub(super) fn prepare_startup(&self, proposed: &Value) -> Preparation {
        self.prepare_startup_with(proposed, link_preparation, sync_directory)
    }

    fn prepare_startup_with(
        &self,
        proposed: &Value,
        install: impl FnOnce(&Path, &Path) -> Result<()>,
        sync_dir: impl FnOnce(&Path) -> Result<()>,
    ) -> Preparation {
        let path = self.run.join("prepared.json");
        let visible = || -> Result<bool> {
            if !has_entry(&path)? {
                return Ok(false);
            }
            ensure!(
                self.signed("prepared")? == *proposed,
                "startup proposal changed"
            );
            Ok(true)
        };
        let undecided = || -> io::Result<bool> {
            for slot in ["startup-decision", "cleanup", "unstarted"] {
                if has_entry(&self.run.join(format!("{slot}.json")))? {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        match visible() {
            Ok(true) => return Preparation::Existing,
            Ok(false) if matches!(undecided(), Ok(true)) => (),
            _ => return Preparation::Uncertain,
        }
        let envelope = self.signed_envelope("prepared", proposed);
        let mut installed = false;
        let result = envelope.and_then(|envelope| {
            publish_with_sync(
                &path,
                &envelope,
                |tmp, path| {
                    install(tmp, path)?;
                    installed = true;
                    Ok(fs::remove_file(tmp)?)
                },
                sync_dir,
            )
        });
        match (visible(), result) {
            // The same authenticated proposal remains authoritative even when
            // the directory sync failed after its name became visible.
            (Ok(true), _) if installed => Preparation::Installed,
            (Ok(true), _) => Preparation::Existing,
            (Ok(false), Err(error)) if !installed && matches!(undecided(), Ok(true)) => {
                Preparation::NotInstalled(error)
            }
            _ => Preparation::Uncertain,
        }
    }

    // Caller holds the unit lock. The original publisher alone calls this
    // method after its owner and assignment checks.
    pub fn commit_ready(&self, prepared: &Value, check: impl FnOnce() -> Result<()>) -> Result<()> {
        ensure!(
            !has_entry(&self.run.join("unstarted.json"))?
                && !has_entry(&self.run.join("cleanup.json"))?,
            "startup already ended"
        );
        ensure!(
            self.install_startup_decision(StartupDecision::Ready, prepared, check)?
                == StartupDecision::Ready,
            "startup was already cancelled"
        );
        Ok(())
    }

    pub fn startup_decision(&self, prepared: &Value) -> Result<Option<StartupDecision>> {
        let Some(body) = self.startup_decision_body()? else {
            return Ok(None);
        };
        ensure!(
            body["data"] == *prepared,
            "startup decision changed the proposal"
        );
        match field(&body, "kind")? {
            "ready" => Ok(Some(StartupDecision::Ready)),
            "startup-abort" => Ok(Some(StartupDecision::Aborted)),
            _ => bail!("invalid startup decision"),
        }
    }

    fn startup_decision_body(&self) -> Result<Option<Value>> {
        let slot = "startup-decision";
        if self.ticket["startupProtocol"] != 1
            || !has_entry(&self.run.join(format!("{slot}.json")))?
        {
            return Ok(None);
        }
        let body = self.signed_body(slot)?;
        ensure!(
            matches!(body["kind"].as_str(), Some("ready" | "startup-abort")),
            "invalid startup decision"
        );
        ensure!(
            body["data"] == self.signed("prepared")?,
            "startup proposal changed"
        );
        Ok(Some(body))
    }

    pub fn abort_startup(&self, prepared: &Value) -> Result<StartupDecision> {
        if let Some(decision) = self.startup_decision(prepared)? {
            return Ok(decision);
        }
        ensure!(
            !has_entry(&self.run.join("unstarted.json"))?
                && !has_entry(&self.run.join("cleanup.json"))?,
            "startup already ended"
        );
        self.install_startup_decision(StartupDecision::Aborted, prepared, || Ok(()))
    }

    fn install_startup_decision(
        &self,
        choice: StartupDecision,
        prepared: &Value,
        check: impl FnOnce() -> Result<()>,
    ) -> Result<StartupDecision> {
        ensure!(
            self.ticket["startupProtocol"] == 1 && self.signed("prepared")? == *prepared,
            "startup proposal changed"
        );
        let kind = if choice == StartupDecision::Ready {
            "ready"
        } else {
            "startup-abort"
        };
        let envelope = self.signed_envelope(kind, prepared)?;
        let path = self.run.join("startup-decision.json");
        let result = publish_with(&path, &envelope, |tmp, path| {
            // A single name commits exactly one choice. Both file contents
            // are synced before the link, which never replaces a decision.
            check()?;
            fs::hard_link(tmp, path)?;
            Ok(fs::remove_file(tmp)?)
        });
        // A failed link can mean the other side committed first. A successful
        // link followed by a failed sync also leaves a verifiable decision.
        if let Some(decision) = self.startup_decision(prepared)? {
            File::open(&self.run)?.sync_all()?;
            return Ok(decision);
        }
        result?;
        bail!("missing startup decision")
    }

    fn ready_body(&self) -> Result<Option<Value>> {
        if self.ticket["startupProtocol"] == 1 {
            let decision = self.startup_decision_body()?;
            return Ok(decision.filter(|body| body["kind"] == "ready"));
        }
        if !has_entry(&self.run.join("ready.json"))? {
            return Ok(None);
        }
        let body = self.signed_body("ready")?;
        ensure!(body["kind"] == "ready", "invalid ready binding");
        Ok(Some(body))
    }

    fn signed_body(&self, slot: &str) -> Result<Value> {
        self.verify_body(read(&self.run.join(format!("{slot}.json")))?)
    }

    fn verify_body(&self, obj: Value) -> Result<Value> {
        let body = &obj["body"];
        verify(
            &self.token,
            &serde_json::to_vec(body)?,
            field(&obj, "signature")?,
        )?;
        ensure!(
            body["version"] == 1 && body["ticket"] == self.ticket,
            "receipt identity mismatch"
        );
        Ok(body.clone())
    }

    pub fn signed(&self, kind: &str) -> Result<Value> {
        let body = if kind == "ready" {
            self.ready_body()?
                .context("execution has no committed ready")?
        } else {
            self.signed_body(kind)?
        };
        ensure!(body["kind"] == kind, "receipt kind mismatch");
        Ok(body["data"].clone())
    }

    fn authorizes_unstarted(&self) -> Result<bool> {
        let info = self.signed("unstarted")?;
        ensure!(self.ready_body()?.is_none(), "execution reached ready");
        if self.run.join("starting.json").try_exists()? {
            let known = self.signed("starting")?;
            ensure!(
                known.get("service").is_none()
                    && info["group_cleanup"] == "removed"
                    && info["path"] == known["containment"]["path"],
                "conflicting launch evidence"
            );
        }
        Ok(matches!(
            info["group_cleanup"].as_str(),
            Some("not-created" | "removed")
        ) || (self.ticket.get("version").is_none()
            && matches!(
                info["reason"].as_str(),
                Some("guardian did not start" | "runtime did not start")
            )))
    }

    fn marked(&self, proof: &Value) -> Result<bool> {
        if !has_entry(&self.run.join("applied.json"))? {
            return Ok(false);
        }
        ensure!(
            self.signed("applied")? == *proof,
            "applied cleanup proof changed"
        );
        // Also seals a rename that was visible after a directory sync error.
        File::open(&self.run)?.sync_all()?;
        Ok(true)
    }

    fn indexed(&self, current: &Value, proof: &Value) -> Result<bool> {
        if self.ticket["version"] != 2 || current["applied"] != true {
            return Ok(false);
        }
        ensure!(
            current["appliedProof"] == digest(&serde_json::to_vec(proof)?),
            "indexed proof changed"
        );
        Ok(true)
    }

    pub fn current_proof(&self) -> Result<Value> {
        let proof = self.proof()?;
        if self.ticket.get("version").is_none() {
            ensure!(self.marked(&proof)?, "legacy application unknown");
        }
        Ok(proof)
    }

    pub fn replaceable(&self, current: &Value) -> bool {
        if self.ticket["version"] == 2 && self.authorizes_unstarted().unwrap_or(false) {
            return true;
        }
        (|| -> Result<bool> {
            let proof = self.proof()?;
            let indexed = self.indexed(current, &proof)?;
            Ok(self.marked(&proof)? && (self.ticket["version"].is_null() || indexed))
        })()
        .unwrap_or(false)
    }

    // The first durable signed copy commits application. The index mirrors it.
    // Called under the unit lock with a verified current proof.
    pub fn reconcile(&self, path: &Path, mut current: Value, proof: Value) -> Result<Value> {
        let indexed = self.indexed(&current, &proof)?;
        let marked = self.marked(&proof)?;
        ensure!(
            self.ticket["version"] == 2 || marked,
            "legacy application unknown"
        );
        let mut result = json!({"result":"uncertain", "proof":proof, "ticket":self.ticket,
                                "recovery_pending":true});
        if !marked && self.save_signed("applied", proof.clone()).is_err() {
            match self.marked(&proof) {
                Ok(true) => (),
                Ok(false) => {
                    result["applied"] = json!(indexed);
                    result["newly_applied"] = json!(false);
                    return Ok(result);
                }
                Err(_) => {
                    if indexed {
                        result["applied"] = json!(true);
                        result["newly_applied"] = json!(false);
                    }
                    return Ok(result);
                }
            }
        }
        result["applied"] = json!(true);
        result["newly_applied"] = json!(!marked && !indexed);
        if current["applied"] != true {
            current["applied"] = json!(true);
            if self.ticket["version"] == 2 {
                current["appliedProof"] = json!(digest(&serde_json::to_vec(&proof)?));
            }
            if publish_current(path, current, self).is_err() {
                return Ok(result);
            }
        } else if File::open(path.parent().context("missing unit")?)
            .and_then(|dir| dir.sync_all())
            .is_err()
        {
            return Ok(result);
        }
        result["result"] = json!("completed");
        result["recovery_pending"] = json!(false);
        Ok(result)
    }

    pub fn proof(&self) -> Result<Value> {
        let receipt = self.signed("cleanup")?;
        let state = field(&receipt, "state")?;
        ensure!(
            receipt["proves"] == "execution-empty"
                && matches!(state, "closed" | "exited" | "faulted" | "startup-aborted"),
            "invalid cleanup state"
        );
        let ready_body = self.ready_body()?;
        let ready = ready_body.is_some();
        let binding = if let Some(body) = ready_body {
            ensure!(
                state != "startup-aborted",
                "committed execution cannot be aborted"
            );
            body
        } else if state == "startup-aborted" {
            let body = self.signed_body("prepared")?;
            ensure!(
                self.startup_decision(&body["data"])? == Some(StartupDecision::Aborted),
                "startup was not confirmed cancelled"
            );
            body
        } else {
            ensure!(state == "faulted", "pre-ready cleanup must be faulted");
            if self.ticket["startupProtocol"] == 1 {
                ensure!(
                    !has_entry(&self.run.join("prepared.json"))?
                        && !has_entry(&self.run.join("startup-decision.json"))?,
                    "bundled startup cleanup has no confirmed decision"
                );
            }
            self.signed_body("starting")?
        };
        let phase = field(&binding, "kind")?;
        ensure!(!ready || phase == "ready", "invalid ready binding");
        let identity = &binding["data"];
        ensure!(
            *identity == receipt["identity"],
            "creation identity mismatch"
        );
        let group = &identity["containment"];
        ensure!(
            process_record(&identity["guardian"])
                && group["kind"] == "cgroup-v2"
                && Path::new(field(group, "path")?).is_absolute()
                && group["device"].is_u64()
                && group["inode"].as_u64().is_some_and(|i| i > 0),
            "missing guardian or containment identity"
        );
        match phase {
            "starting" => ensure!(
                state == "faulted" && identity.get("service").is_none_or(process_record),
                "invalid pre-ready cleanup"
            ),
            "ready" | "prepared" => ensure!(
                (phase == "ready" || state == "startup-aborted")
                    && ["service", "kernel", "worker"]
                        .iter()
                        .all(|k| process_record(&identity[*k]))
                    && uuid::Uuid::parse_str(field(&identity, "kernel_generation")?).is_ok()
                    && identity["endpoint"]
                        .as_u64()
                        .is_some_and(|p| (1..=65535).contains(&p)),
                "incomplete ready identity"
            ),
            _ => bail!("invalid creation binding"),
        }
        Ok(receipt)
    }
}

fn process_record(v: &Value) -> bool {
    v["pid"]
        .as_u64()
        .is_some_and(|p| (1..=u32::MAX as u64).contains(&p))
        && v["start_ticks"].is_u64()
        && v["boot"]
            .as_str()
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
}

fn has_entry(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// An older writer can install a successor without retiring a signed index.
// Every other generation must be on one authenticated path to the selected tip.
pub fn ordered(unit: &Path, selected: &Store) -> Result<bool> {
    if has_entry(&selected.run.join("superseded.json"))? {
        return Ok(false);
    }
    let mut predecessors = std::collections::HashMap::new();
    for entry in fs::read_dir(unit)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "current.json" || name == "lock" || entry.path() == selected.run {
            continue;
        }
        if selected.ticket["version"] != 2 || !entry.file_type()?.is_dir() {
            return Ok(false);
        }
        let envelope = read(&entry.path().join("superseded.json"))?;
        let ticket = &envelope["body"]["ticket"];
        let generation = field(ticket, "generation")?.to_owned();
        ensure!(
            name.to_str() == Some(&generation) && ticket["unit"] == selected.ticket["unit"],
            "misplaced retirement record"
        );
        let prior = Store::open(unit, ticket)?;
        let body = prior.verify_body(envelope)?;
        ensure!(body["kind"] == "superseded", "invalid retirement record");
        let successor = field(&body["data"], "generation")?.to_owned();
        if predecessors.insert(successor, generation).is_some() {
            return Ok(false);
        }
    }
    let mut generation = field(&selected.ticket, "generation")?.to_owned();
    while let Some(previous) = predecessors.remove(&generation) {
        generation = previous;
    }
    Ok(predecessors.is_empty())
}

fn verify_index(mut record: Value, store: &Store) -> Result<Value> {
    ensure!(
        record["ticket"]["version"] == 2,
        "unsupported current ticket"
    );
    let tag = record
        .as_object_mut()
        .context("invalid current index")?
        .remove("indexSignature")
        .context("unsigned current index")?;
    verify(
        &store.token,
        &serde_json::to_vec(&json!(["current", record]))?,
        tag.as_str().context("invalid current signature")?,
    )?;
    Ok(record)
}

// Mutating callers hold the unit lock. Unlocked readers must recheck the
// selected ticket before relying on receipts. Only a unit containing the lock alone is new.
pub fn current(unit: &Path) -> Result<Option<(Value, Store)>> {
    let path = unit.join("current.json");
    if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        for entry in fs::read_dir(unit)? {
            ensure!(
                entry?.file_name() == "lock",
                "missing current index with existing history"
            );
        }
        return Ok(None);
    }
    let mut record = read(&path)?;
    let ticket = &record["ticket"];
    ensure!(
        unit.file_name() == Some(std::ffi::OsStr::new(field(ticket, "unit")?))
            && valid_name(field(ticket, "attempt")?)
            && uuid::Uuid::parse_str(field(ticket, "nonce")?).is_ok()
            && record["applied"].is_boolean()
            && process_record(&record["creator"])
            && record["pending_until_ms"].is_u64(),
        "invalid current index"
    );
    let store = Store::open(unit, ticket)?;
    let backup = store.run.join("index.json");
    if ticket.get("version").is_some()
        || record.get("indexSignature").is_some()
        || has_entry(&backup)?
    {
        record = verify_index(record, &store)?;
        let original = verify_index(read(&backup)?, &store)?;
        for key in ["ticket", "creator", "pending_until_ms"] {
            ensure!(original[key] == record[key], "generation index mismatch");
        }
    } else {
        // A pending record without independent format or identity evidence
        // could also be a damaged new-format index.
        let mut bound = false;
        for slot in ["ready", "cleanup", "unstarted", "starting"] {
            if has_entry(&store.run.join(format!("{slot}.json")))? {
                let body = store.signed_body(slot)?;
                ensure!(
                    body["kind"] == slot || (slot == "starting" && body["kind"] == "ready"),
                    "legacy binding kind mismatch"
                );
                bound = true;
                break;
            }
        }
        ensure!(bound, "unverified legacy ticket");
    }
    Ok(Some((record, store)))
}

pub fn publish_current(path: &Path, mut record: Value, store: &Store) -> Result<()> {
    if store.ticket["version"] == 2 {
        record["indexSignature"] = json!(sign(
            &store.token,
            &serde_json::to_vec(&json!(["current", record]))?
        ));
    }
    publish(path, &record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(unit: &Path, legacy: bool) -> Result<Store> {
        let token = fresh();
        let mut ticket = json!({"unit":unit.file_name().unwrap().to_str(), "attempt":"one",
            "generation":fresh(), "nonce":fresh(), "capabilityHash":digest(token.as_bytes())});
        if !legacy {
            ticket["version"] = json!(2);
        }
        let run = unit.join(field(&ticket, "generation")?);
        private_dir(unit)?;
        private_dir(&run)?;
        publish(&run.join("key.json"), &json!(token))?;
        Store::open(unit, &ticket)
    }

    #[test]
    fn pending_index_keeps_its_format() -> Result<()> {
        let unit = std::env::temp_dir().join(fresh());
        let store = generation(&unit, false)?;
        let record = json!({"ticket":store.ticket, "applied":false,
            "creator":identity(std::process::id())?, "pending_until_ms":1});
        let path = unit.join("current.json");
        let backup = store.run.join("index.json");
        publish_current(&backup, record, &store)?;
        let mut damaged = read(&backup)?;
        damaged["ticket"].as_object_mut().unwrap().remove("version");
        publish(&path, &damaged)?;
        assert!(current(&unit).is_err());
        damaged.as_object_mut().unwrap().remove("indexSignature");
        publish(&path, &damaged)?;
        assert!(current(&unit).is_err());
        fs::copy(&backup, &path)?;
        assert_eq!(current(&unit)?.unwrap().1.ticket, store.ticket);
        fs::remove_dir_all(unit)?;
        Ok(())
    }

    #[test]
    fn interrupted_ready_keeps_its_binding() -> Result<()> {
        let unit = std::env::temp_dir().join(fresh());
        let store = generation(&unit, false)?;
        let process = identity(std::process::id())?;

        let actual = json!({"guardian":process, "service":process, "kernel":process,
            "worker":process,"endpoint":42,"kernel_generation":fresh(),
            "containment":{"kind":"cgroup-v2","path":"/test-fixture","device":1,"inode":2}});
        let ready = store.run.join("ready.json");
        fs::create_dir(&ready)?;
        assert!(store.save_signed("ready", actual.clone()).is_err());
        let mut receipt = json!({"proves":"execution-empty","state":"faulted","identity":actual});
        store.save_signed("cleanup", receipt.clone())?;
        assert!(store.proof().is_err());
        fs::remove_dir(&ready)?;
        assert!(store.signed("ready").is_err());
        assert_eq!(store.proof()?, receipt);
        receipt["state"] = json!("closed");
        store.save_signed("cleanup", receipt)?;
        assert!(store.proof().is_err());
        fs::remove_dir_all(unit)?;
        Ok(())
    }

    #[test]
    fn managed_startup_keeps_the_first_signed_decision() -> Result<()> {
        let root = std::env::temp_dir().join(fresh());
        let task = || -> Result<()> {
            for first in [StartupDecision::Ready, StartupDecision::Aborted] {
                let unit = root.join(fresh());
                let mut store = generation(&unit, false)?;
                store.ticket["startupProtocol"] = json!(1);
                let process = identity(std::process::id())?;
                let proposed = json!({"guardian":process,"service":process,"kernel":process,
                    "worker":process,"endpoint":42,"kernel_generation":fresh(),
                    "containment":{"kind":"cgroup-v2","path":"/fixture","device":1,"inode":2}});
                store.save_signed("prepared", proposed.clone())?;
                assert!(store.signed("ready").is_err());
                let mut wrong = proposed.clone();
                wrong["endpoint"] = json!(43);
                assert!(store.commit_ready(&wrong, || Ok(())).is_err());
                assert!(store
                    .commit_ready(&proposed, || bail!("fixture authorization expired"))
                    .is_err());
                assert_eq!(store.startup_decision(&proposed)?, None);
                if first == StartupDecision::Ready {
                    store.commit_ready(&proposed, || Ok(()))?;
                    assert_eq!(store.abort_startup(&proposed)?, first);
                    assert_eq!(store.signed("ready")?, proposed);
                } else {
                    assert_eq!(store.abort_startup(&proposed)?, first);
                    assert!(store.commit_ready(&proposed, || Ok(())).is_err());
                    assert!(store.signed("ready").is_err());
                }
                let state = if first == StartupDecision::Ready {
                    "closed"
                } else {
                    "startup-aborted"
                };
                let cleanup = json!({"proves":"execution-empty","state":state,"identity":proposed});
                store.save_signed("cleanup", cleanup.clone())?;
                assert_eq!(store.proof()?, cleanup);
            }
            Ok(())
        };
        fs::create_dir(&root)?;
        let result = task();
        fs::remove_dir_all(root)?;
        result
    }

    #[test]
    fn managed_preparation_publication_preserves_the_startup_decision() -> Result<()> {
        let root = std::env::temp_dir().join(fresh());
        let exercise = || -> Result<()> {
            let proposal = json!({"guardian":{"pid":7,"boot":fresh(),"start_ticks":5},
                "service":{"pid":8,"boot":fresh(),"start_ticks":6},
                "kernel":{"pid":9,"boot":fresh(),"start_ticks":7},
                "worker":{"pid":10,"boot":fresh(),"start_ticks":8},
                "endpoint":42,"kernel_generation":fresh(),
                "containment":{"kind":"cgroup-v2","path":"/fixture","device":1,"inode":2}});
            let fixture = || -> Result<Store> {
                let mut store = generation(&root.join(fresh()), false)?;
                store.ticket["startupProtocol"] = json!(1);
                Ok(store)
            };
            let expired = json!({"deadline":0});
            let poll = |store: &Store, may_abort: &mut bool| {
                super::super::user_service::poll_startup(store, &expired, &proposal, may_abort)
            };
            let before = fixture()?;
            assert!(matches!(
                before.prepare_startup_with(&proposal, |_, _| bail!("install failed"), |_| Ok(())),
                Preparation::NotInstalled(_)
            ));
            assert!(before.signed("prepared").is_err());
            assert!(before.commit_ready(&proposal, || Ok(())).is_err());

            let after = fixture()?;
            assert!(matches!(
                after.prepare_startup_with(&proposal, link_preparation, |_| bail!("sync failed")),
                Preparation::Installed
            ));
            assert_eq!(after.signed("prepared")?, proposal);
            assert_eq!(poll(&after, &mut true)?, Some(StartupDecision::Aborted));
            assert!(after.commit_ready(&proposal, || Ok(())).is_err());
            let decision_path = after.run.join("startup-decision.json");
            let saved_abort = read(&decision_path)?;
            publish(&decision_path, &json!({"fixture":"unreadable decision"}))?;
            let mut may_abort = true;
            assert!(poll(&after, &mut may_abort).is_err());
            assert!(!may_abort);
            fs::remove_file(&decision_path)?;
            assert_eq!(poll(&after, &mut may_abort)?, None);
            publish(&decision_path, &saved_abort)?;
            assert_eq!(
                poll(&after, &mut may_abort)?,
                Some(StartupDecision::Aborted)
            );
            let aborted =
                json!({"proves":"execution-empty","state":"startup-aborted","identity":proposal});
            after.save_signed("cleanup", aborted.clone())?;
            assert_eq!(after.proof()?, aborted);

            let committed = fixture()?;
            assert!(matches!(
                committed.prepare_startup_with(&proposal, link_preparation, |_| {
                    committed.commit_ready(&proposal, || Ok(()))?;
                    bail!("sync failed after commit")
                }),
                Preparation::Installed
            ));
            assert!(matches!(
                committed.prepare_startup(&proposal),
                Preparation::Existing
            ));
            assert_eq!(poll(&committed, &mut false)?, Some(StartupDecision::Ready));
            let mut other = proposal.clone();
            other["endpoint"] = json!(43);
            assert!(matches!(
                committed.prepare_startup(&other),
                Preparation::Uncertain
            ));
            assert_eq!(committed.signed("prepared")?, proposal);
            let closed = json!({"proves":"execution-empty","state":"closed","identity":proposal});
            committed.save_signed("cleanup", closed.clone())?;
            assert_eq!(committed.proof()?, closed);
            // Losing either half of the binding must not invent a decision or proof.
            let decision_path = committed.run.join("startup-decision.json");
            let saved_ready = read(&decision_path)?;
            fs::remove_file(&decision_path)?;
            assert!(matches!(
                committed.prepare_startup(&proposal),
                Preparation::Existing
            ));
            assert_eq!(poll(&committed, &mut false)?, None);
            assert!(committed.abort_startup(&proposal).is_err());
            assert!(committed.signed("ready").is_err());
            assert!(committed.proof().is_err());
            publish(&decision_path, &saved_ready)?;
            assert_eq!(committed.proof()?, closed);
            fs::remove_file(committed.run.join("prepared.json"))?;
            assert!(matches!(
                committed.prepare_startup(&proposal),
                Preparation::Uncertain
            ));
            assert!(committed.signed("ready").is_err());
            assert!(committed.proof().is_err());
            Ok(())
        };
        fs::create_dir(&root)?;
        let result = exercise();
        fs::remove_dir_all(root)?;
        result
    }

    #[test]
    fn mixed_history_requires_one_signed_chain_ending_at_the_selected_generation() -> Result<()> {
        let unit = std::env::temp_dir().join(fresh());
        let legacy = generation(&unit, true)?;
        let new = generation(&unit, false)?;
        assert!(!ordered(&unit, &new).unwrap_or(false));
        let retire = |from: &Store, to: &Store| {
            from.save_signed("superseded", json!({"generation":to.ticket["generation"]}))
        };
        retire(&legacy, &new)?;
        assert!(ordered(&unit, &new)?);
        let latest = generation(&unit, false)?;
        retire(&new, &latest)?;
        assert!(ordered(&unit, &latest)?);
        assert!(!ordered(&unit, &new).unwrap_or(false));
        retire(&legacy, &latest)?;
        assert!(!ordered(&unit, &latest).unwrap_or(false));
        retire(&legacy, &new)?;
        retire(&new, &legacy)?;
        assert!(!ordered(&unit, &latest).unwrap_or(false));
        fs::copy(
            new.run.join("superseded.json"),
            legacy.run.join("superseded.json"),
        )?;
        assert!(!ordered(&unit, &latest).unwrap_or(false));
        fs::remove_dir_all(unit)?;
        Ok(())
    }

    #[test]
    fn legacy_application_and_order_need_independent_evidence() -> Result<()> {
        let unit = std::env::temp_dir().join(fresh());
        let first = generation(&unit, true)?;
        let actual = json!({"guardian":identity(std::process::id())?,
            "containment":{"kind":"cgroup-v2","path":"/fixture","device":1,"inode":2}});
        let proof = json!({"proves":"execution-empty","state":"faulted","identity":actual});
        first.save_signed("starting", actual.clone())?;
        first.save_signed("cleanup", proof.clone())?;
        assert!(ordered(&unit, &first)?);
        assert!(first.current_proof().is_err());
        for bit in [false, true] {
            assert!(!first.replaceable(&json!({"applied":bit})));
        }
        first.save_signed("applied", proof.clone())?;
        assert_eq!(first.current_proof()?, proof);
        assert!(first.replaceable(&json!({"applied":false})));
        let second = generation(&unit, true)?;
        second.save_signed("starting", actual)?;
        second.save_signed("cleanup", proof)?;
        assert!(second.proof().is_ok());
        for run in [&first, &second] {
            assert!(!ordered(&unit, run)?);
        }
        fs::remove_dir_all(unit)?;
        Ok(())
    }
}
