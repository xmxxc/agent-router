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
    let tmp = path.with_extension(fresh());
    let mut file = new_file(&tmp)?;
    let result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, value)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(path.parent().context("missing parent")?)?.sync_all()?;
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
    let grant = json!([stage, spec]);
    let signature = sign(token, &serde_json::to_vec(&grant)?);
    serde_json::to_writer(&mut input, &json!({"spec":spec,"signature":signature}))?;
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
        // Promote the starting binding by rename. A ready file only appears
        // once complete, and no weaker binding survives that transition.
        let slot = if kind == "ready" { "starting" } else { kind };
        let body = json!({"version":1,"kind":kind,"ticket":self.ticket,"data":data});
        let mac = sign(&self.token, &serde_json::to_vec(&body)?);
        let path = self.run.join(format!("{slot}.json"));
        publish(&path, &json!({"body":body,"signature":mac}))?;
        if kind == "ready" {
            fs::rename(path, self.run.join("ready.json"))?;
            File::open(&self.run)?.sync_all()?;
        }
        Ok(())
    }

    fn signed_body(&self, slot: &str) -> Result<Value> {
        let obj = read(&self.run.join(format!("{slot}.json")))?;
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
        let body = self.signed_body(kind)?;
        ensure!(body["kind"] == kind, "receipt kind mismatch");
        Ok(body["data"].clone())
    }

    fn authorizes_unstarted(&self) -> Result<bool> {
        let info = self.signed("unstarted")?;
        ensure!(
            !self.run.join("ready.json").try_exists()?,
            "execution reached ready"
        );
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

    pub fn replaceable(&self, current: &Value) -> bool {
        self.authorizes_unstarted().unwrap_or(false)
            || self
                .proof()
                .and_then(|proof| self.applied(current, &proof))
                .unwrap_or(false)
    }

    pub fn applied(&self, current: &Value, proof: &Value) -> Result<bool> {
        let indexed = current["applied"] == true;
        if indexed && self.ticket["version"] == 2 {
            ensure!(
                current["appliedProof"] == digest(&serde_json::to_vec(proof)?),
                "indexed proof changed"
            );
        }
        if fs::symlink_metadata(self.run.join("applied.json"))
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        {
            return Ok(indexed);
        }
        ensure!(
            self.signed("applied")? == *proof,
            "applied cleanup proof changed"
        );
        Ok(true)
    }

    pub fn proof(&self) -> Result<Value> {
        let receipt = self.signed("cleanup")?;
        let state = field(&receipt, "state")?;
        ensure!(
            receipt["proves"] == "execution-empty"
                && matches!(state, "closed" | "exited" | "faulted"),
            "invalid cleanup state"
        );
        let phase = if fs::symlink_metadata(self.run.join("ready.json"))
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        {
            "starting"
        } else {
            "ready"
        };
        let identity = self.signed(phase)?;
        ensure!(
            identity == receipt["identity"],
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
            "ready" => ensure!(
                ["service", "kernel", "worker"]
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

// The first format had an unsigned index and no supersession marker. An
// old copy may point to an already-cleaned generation while a sibling runs.
pub fn check_legacy_siblings(unit: &Path, selected: &Store) -> Result<()> {
    if selected.ticket.get("version").is_some() {
        return Ok(());
    }
    for entry in fs::read_dir(unit)? {
        let entry = entry?;
        let dir = entry.path();
        if dir == selected.run || !entry.file_type()?.is_dir() {
            continue;
        }
        let slot = if dir.join("cleanup.json").try_exists()? {
            "cleanup"
        } else {
            "unstarted"
        };
        let file = read(&dir.join(format!("{slot}.json")))?;
        let ticket = &file["body"]["ticket"];
        ensure!(
            ticket["unit"] == selected.ticket["unit"]
                && Some(std::ffi::OsStr::new(field(ticket, "generation")?)) == dir.file_name(),
            "unexpected generation"
        );
        let sibling = Store::open(unit, ticket)?;
        ensure!(
            sibling.proof().is_ok() || sibling.authorizes_unstarted().unwrap_or(false),
            "another generation lacks terminal evidence"
        );
    }
    Ok(())
}

// Called with the unit lock held. Only a unit containing the lock alone is new.
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
    ensure!(
        !store.run.join("superseded.json").try_exists()?,
        "superseded current index"
    );
    if ticket.get("version").is_some() {
        ensure!(ticket["version"] == 2, "unsupported current ticket");
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
    }
    if store.ticket.get("version").is_none() {
        for slot in ["ready", "cleanup", "unstarted"] {
            if store.run.join(format!("{slot}.json")).try_exists()? {
                store.signed(slot)?;
                break;
            }
        }
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
