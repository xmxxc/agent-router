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
        let body = json!({"version":1,"kind":kind,"ticket":self.ticket,"data":data});
        let mac = sign(&self.token, &serde_json::to_vec(&body)?);
        publish(
            &self.run.join(format!("{kind}.json")),
            &json!({"body":body,"signature":mac}),
        )
    }

    pub fn signed(&self, kind: &str) -> Result<Value> {
        let obj = read(&self.run.join(format!("{kind}.json")))?;
        let body = &obj["body"];
        verify(
            &self.token,
            &serde_json::to_vec(body)?,
            field(&obj, "signature")?,
        )?;
        ensure!(
            body["version"] == 1 && body["kind"] == kind && body["ticket"] == self.ticket,
            "receipt identity mismatch"
        );
        Ok(body["data"].clone())
    }

    pub fn proof(&self) -> Result<Value> {
        let receipt = self.signed("cleanup")?;
        ensure!(receipt["proves"] == "execution-empty", "cleanup unknown");
        Ok(receipt)
    }
}
