use super::cleanup_receipt;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, DirBuilder},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const DISPATCH: &str = "d-1a2b3c4d";
const ATTEMPT: &str = "a-012345abcdef";
const AUDIT: u64 = 7;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("wfc-cleanup-{}", uuid::Uuid::new_v4()));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(fs::canonicalize(path)?))
    }

    fn create(&self, root: &Path, argv: Value) -> Result<Value> {
        let binary = std::env::var_os("WFC_TEST_BINARY").context("set WFC_TEST_BINARY")?;
        let mut child = Command::new(binary)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("WFC_STATE", &self.0)
            .env("WFC_BUNDLED_CGROUP_ROOT", root)
            .arg("--bundled")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let request = json!({
            "op":"create","unit":DISPATCH,"attempt":ATTEMPT,"cwd":self.0,"argv":argv,
            "env":{"PATH":"/usr/bin:/bin"}
        });
        let written = writeln!(
            child.stdin.take().context("missing fixture input")?,
            "{request}"
        );
        let output = child.wait_with_output()?;
        written?;
        ensure!(output.status.success(), "bundled fixture launcher failed");
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // If an assertion fails earlier, keep the private state available for
        // the worker's bounded natural exit and the guardian's cleanup.
        let _ = fs::write(self.0.join("exit"), b"");
    }
}

fn wait_for(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.try_exists()? {
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

#[test]
#[ignore = "requires a separately built Linux wfc"]
fn cleanup_receipt_ordinary_directory() -> Result<()> {
    let fixture = Fixture::new()?;
    let result = fixture.create(&fixture.0, json!(["/bin/touch", "started"]))?;
    assert_eq!(result["result"], "definitely-not-sent");
    assert!(!fixture.0.join("started").exists());
    println!("\nBUNDLED_CLEANUP_RECEIPT_ORDINARY_DIRECTORY_REJECTED");
    fs::remove_dir_all(&fixture.0)?;
    Ok(())
}

#[test]
#[ignore = "requires an explicitly authorized disposable cgroup v2 delegation and Linux wfc"]
fn cleanup_receipt_registered_create() -> Result<()> {
    let Some(root) = std::env::var_os("WFC_TEST_DELEGATION").and_then(|p| fs::canonicalize(p).ok())
    else {
        println!("\nBUNDLED_CLEANUP_RECEIPT_UNVERIFIED: no disposable delegation supplied");
        return Ok(());
    };
    if root == Path::new("/sys/fs/cgroup") {
        println!("\nBUNDLED_CLEANUP_RECEIPT_UNVERIFIED: mount root is outside this fixture");
        return Ok(());
    }
    let me = unsafe { libc::geteuid() };
    let procs = root.join("cgroup.procs");
    let pid = std::process::id().to_string();
    let delegated = root.join("cgroup.controllers").is_file()
        && root.join("cgroup.kill").is_file()
        && fs::metadata(&root).is_ok_and(|m| m.uid() == me && m.mode() & 0o300 == 0o300)
        && fs::OpenOptions::new().write(true).open(&procs).is_ok()
        && fs::read_to_string(&procs).is_ok_and(|pids| pids.lines().any(|line| line == pid));
    if !delegated {
        println!(
            "\nBUNDLED_CLEANUP_RECEIPT_UNVERIFIED: fixture is not running inside the supplied writable delegation"
        );
        return Ok(());
    }
    let fixture = Fixture::new()?;
    let created = fixture.create(
        &root,
        json!([
            "/bin/sh", "-c",
            "echo started > started; for i in $(seq 1 600); do [ ! -f exit ] || exit 0; sleep .05; done"
        ]),
    )?;
    ensure!(
        created["result"] == "accepted",
        "fixture did not reach ready: {created}"
    );
    wait_for(&fixture.0.join("started"))?;
    let ticket = &created["ticket"];
    let identity = &created["identity"];
    let group = Path::new(
        identity["containment"]["path"]
            .as_str()
            .context("missing group path")?,
    );
    assert_eq!(group.parent(), Some(root.as_path()));
    let group_stat = fs::metadata(group)?;
    assert_eq!(identity["containment"]["device"], group_stat.dev());
    assert_eq!(identity["containment"]["inode"], group_stat.ino());
    let dir = fixture.0.join("dispatches");
    fs::create_dir(&dir)?;
    let record = dir.join(format!("{DISPATCH}.json"));
    let mut row = json!({
        "dispatch":DISPATCH,"backend":"bundled","host":"codex","scope":"bundled-fixture",
        "attempt":ATTEMPT,"round":1,
        "create":{"dispatch":DISPATCH,"attempt":ATTEMPT,"round":1,
                  "result":"uncertain","ticket":ticket,"identity":identity},
        "audit":[{"id":AUDIT,"dispatch":DISPATCH,"operation":"create","round":1,
                  "attempt":ATTEMPT,"toAttempt":ATTEMPT,"toHost":"codex","result":"uncertain"}]
    });
    let publish =
        |row: &Value| -> Result<()> { super::super::super::write_json(&record, row, None) };
    publish(&row)?;
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT)?.is_none());

    fs::write(fixture.0.join("exit"), b"")?;
    let run = fixture.0.join("bundled").join(DISPATCH).join(
        ticket["generation"]
            .as_str()
            .context("missing generation")?,
    );
    let receipt_file = run.join("cleanup.json");
    wait_for(&receipt_file)?;
    assert!(!group.try_exists()?);
    let original = fs::read(&receipt_file)?;
    let signed: Value = serde_json::from_slice(&original)?;
    assert_eq!(&signed["body"]["ticket"], ticket);
    assert_eq!(&signed["body"]["data"]["identity"], identity);
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&signed["body"])?));
    let index = run
        .parent()
        .context("missing private unit")?
        .join("current.json");
    let registered = fs::read(&record)?;
    let current = fs::read(&index)?;
    let receipt = cleanup_receipt(&fixture.0, &row, AUDIT)?.context("expected verified cleanup")?;
    assert_eq!(
        receipt,
        json!({
            "dispatch":DISPATCH,"scope":"bundled-fixture","round":1,"attempt":ATTEMPT,"audit":AUDIT,
            "proof":{"unit":DISPATCH,"generation":ticket["generation"],"attempt":ATTEMPT,
                     "nonce":ticket["nonce"],"state":"exited","proves":"execution-empty",
                     "cleanup":"confirmed","receiptSha256":digest}
        })
    );
    assert_eq!(
        cleanup_receipt(&fixture.0, &row, AUDIT)?,
        Some(receipt.clone())
    );
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT + 1)?.is_none());
    assert_eq!(fs::read(&record)?, registered);
    assert_eq!(fs::read(&index)?, current);
    assert_eq!(fs::read(&receipt_file)?, original);
    assert!(!run.join("applied.json").exists());

    row["audit"][0]["result"] = json!("unknown");
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT)?.is_none());
    publish(&row)?;
    assert_eq!(
        cleanup_receipt(&fixture.0, &row, AUDIT)?,
        Some(receipt.clone())
    );
    let mut duplicate_audit = row["audit"][0].clone();
    duplicate_audit["id"] = json!(AUDIT + 1);
    row["audit"]
        .as_array_mut()
        .context("missing audits")?
        .push(duplicate_audit);
    publish(&row)?;
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT)?.is_none());
    row["audit"].as_array_mut().context("missing audits")?.pop();
    row["audit"][0]["toAttempt"] = json!("a-000000000000");
    publish(&row)?;
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT)?.is_none());
    row["audit"][0]["toAttempt"] = json!(ATTEMPT);
    row["create"]["identity"]["worker"] = Value::Null;
    publish(&row)?;
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT)?.is_none());
    row["create"]["identity"] = identity.clone();
    row["backendHandle"] = ticket.clone();
    row["backendHandle"]["nonce"] = json!(uuid::Uuid::new_v4().to_string());
    publish(&row)?;
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT)?.is_none());
    row.as_object_mut()
        .context("missing assignment")?
        .remove("backendHandle");
    row["create"]["ticket"] = Value::Null;
    publish(&row)?;
    assert!(cleanup_receipt(&fixture.0, &row, AUDIT)?.is_none());
    assert_eq!(fs::read(&receipt_file)?, original);
    assert_eq!(fs::read(&index)?, current);
    println!("\nBUNDLED_CLEANUP_RECEIPT_VERIFIED {receipt}");
    fs::remove_dir_all(&fixture.0)?;
    Ok(())
}
