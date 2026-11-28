//! One detached runtime owns the Job and the ConPTY kernel for an assignment.
use super::Launch;
use crate::{
    windows_process::{self as process, Job, TICK},
    workflow::{
        assignment, controller, messages, record_scope, valid_id, valid_identity,
        writable_state_dir, write_json,
    },
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const LIMIT: Duration = Duration::from_secs(8);
pub(crate) const CLOSE_USAGE: &str = "wfc close DISPATCH [--owner-token TOKEN]";

struct Execution {
    dir: PathBuf,
    ticket: Value,
}

impl Execution {
    fn new(state: &Path, row: &Value) -> Result<Self> {
        let dispatch = row["dispatch"]
            .as_str()
            .filter(|s| valid_id(s))
            .context("invalid dispatch")?;
        let attempt = row["attempt"]
            .as_str()
            .filter(|s| valid_identity(s, "a-"))
            .context("invalid attempt")?;
        let round = row["round"]
            .as_u64()
            .filter(|round| *round != 0)
            .context("invalid round")?;
        let generation = row["windowsExecution"]
            .as_str()
            .context("missing Windows execution")?;
        ensure!(
            Uuid::parse_str(generation)?.simple().to_string() == generation,
            "invalid Windows execution"
        );
        let ticket = json!({"version":1,"platform":"windows","unit":dispatch,
            "attempt":attempt,"round":round,"generation":generation});
        ensure!(
            row["backendHandle"].is_null() || row["backendHandle"] == ticket,
            "Windows execution mismatch"
        );
        Ok(Self {
            dir: state.join("windows-bundled").join(generation),
            ticket,
        })
    }

    fn read(&self, file: &str) -> Result<Option<Value>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(process::OPEN_REPARSE_POINT)
            .open(self.dir.join(file))
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            file.metadata()?.is_file(),
            "invalid Windows execution evidence"
        );
        let evidence: Value = serde_json::from_reader(file)?;
        ensure!(
            evidence["ticket"] == self.ticket,
            "Windows execution evidence mismatch"
        );
        Ok(Some(evidence))
    }

    fn write(&self, file: &str, mut evidence: Value) -> Result<()> {
        evidence["ticket"] = self.ticket.clone();
        write_json(&self.dir.join(file), &evidence, None)
    }

    fn failure(&self, error: &anyhow::Error) -> Result<()> {
        let category = match error.downcast_ref::<io::Error>().map(io::Error::kind) {
            Some(io::ErrorKind::NotFound) => "executable_missing",
            Some(io::ErrorKind::PermissionDenied) => "permission_denied",
            _ => "spawn_failed",
        };
        self.write("finished.json", json!({
            "result":"definitely-not-sent","category":category,"diagnostic":format!("{error:#}"),
            "alive":false,"resourceCleanup":"confirmed","jobActiveProcesses":0,
            "worker":{"state":"not_started","exit_code":null,"signal":null}
        }))
    }

    fn terminal(&self) -> Result<Option<Value>> {
        let Some(mut value) = self.read("finished.json")? else {
            return Ok(None);
        };
        ensure!(
            matches!(
                value["result"].as_str(),
                Some("completed" | "definitely-not-sent")
            ) && value["jobActiveProcesses"] == 0
                && value["resourceCleanup"] == "confirmed"
                && value["alive"] == false,
            "invalid Windows cleanup evidence"
        );
        if value["result"] == "completed" {
            let ready = self
                .read("ready.json")?
                .context("missing Windows startup evidence")?;
            ensure!(
                value["identity"] == ready["identity"],
                "Windows cleanup identity mismatch"
            );
        }
        value["operation"] = json!("close");
        value["dispatch"] = self.ticket["unit"].clone();
        value["attempt"] = self.ticket["attempt"].clone();
        value["round"] = self.ticket["round"].clone();
        value["closeOperationId"] = json!(format!(
            "close:{}",
            self.ticket["attempt"].as_str().unwrap()
        ));
        Ok(Some(value))
    }

    fn unknown(&self) -> Value {
        json!({"result":"uncertain","ticket":self.ticket,"alive":null,"resourceCleanup":"uncertain"})
    }

    fn verify_launch(&self, row: &Value, ready: &Value) -> Result<()> {
        if row["create"]["result"] == "accepted" {
            ensure!(
                row["create"]["ticket"] == self.ticket
                    && row["create"]["identity"] == ready["identity"],
                "Windows launch identity mismatch"
            );
        }
        Ok(())
    }

    fn startup(&self, runtime: &Value) -> Result<Option<Value>> {
        // A fast worker can publish both ready and terminal before create
        // has observed either. Its create outcome is still accepted.
        if let Some(finished) = self.terminal()? {
            if finished["result"] == "definitely-not-sent" {
                return Ok(Some(finished));
            }
        }
        let ready = self.read("ready.json")?;
        if let Some(ready) = &ready {
            ensure!(
                ready["result"] == "accepted" && ready["identity"]["runtime"] == *runtime,
                "Windows startup identity mismatch"
            );
        }
        Ok(ready)
    }
}

pub(super) fn create(
    state: &Path,
    row: &Value,
    launch: Launch,
    owner: Option<&str>,
) -> Result<Value> {
    let execution = Execution::new(state, row)?;
    let scope = record_scope(row).context("invalid dispatch scope")?;
    let scope_lock = controller::lock_scope(state, scope)?;
    let ledger = messages::lock_for_publish(state)?;
    controller::Owners::read(state)?.require(scope, owner, "dispatch")?;
    let id = row["dispatch"].as_str().context("missing dispatch")?;
    let mut reserved = assignment::public_record(&state.join("dispatches"), id)?
        .context("missing dispatch reservation")?;
    ensure!(
        reserved
            .as_object_mut()
            .context("invalid dispatch reservation")?
            .remove("createPending")
            == Some(Value::Bool(true))
            && reserved == *row,
        "dispatch reservation changed before Windows launch"
    );
    fs::create_dir_all(
        execution
            .dir
            .parent()
            .context("missing runtime directory")?,
    )?;
    fs::create_dir(&execution.dir)?;
    let mut command = Command::new(env::current_exe()?);
    controller::strip_credentials(&mut command);
    command
        .arg("--windows-bundled")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match process::background(&mut command) {
        Ok(child) => child,
        Err(error) if error.is::<process::UnverifiedStageExit>() => return Ok(execution.unknown()),
        Err(error) => {
            execution.failure(&error)?;
            return Ok(execution.terminal()?.unwrap());
        }
    };
    let request = json!({"state":state,"dispatch":id,"attempt":row["attempt"],"round":row["round"],
        "windowsExecution":row["windowsExecution"],"cwd":row["cwd"],"argv":launch.argv,"env":launch.env});
    let runtime = match process::identity(child.id(), child.as_raw_handle()) {
        Ok(identity) => identity,
        Err(_) => return Ok(execution.unknown()),
    };
    let input = child.stdin.take().context("missing runtime input")?;
    let (written, writing) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = written.send(send(input, &request));
    });
    if !matches!(writing.recv_timeout(LIMIT), Ok(Ok(()))) {
        return Ok(execution.unknown());
    }
    drop(ledger);
    drop(scope_lock);
    let deadline = Instant::now() + LIMIT;
    loop {
        match execution.startup(&runtime) {
            Ok(None) => (),
            Ok(Some(answer)) => return Ok(answer),
            Err(_) => return Ok(execution.unknown()),
        }
        if Instant::now() >= deadline || !matches!(child.try_wait(), Ok(None)) {
            // Recheck after the process handle signals. Its last publication
            // can race the first filesystem read in this iteration.
            return Ok(execution
                .startup(&runtime)
                .ok()
                .flatten()
                .unwrap_or_else(|| execution.unknown()));
        }
        thread::sleep(TICK);
    }
}

fn send(mut writer: impl Write, value: &Value) -> Result<()> {
    let mut data = serde_json::to_vec(value)?;
    data.push(b'\n');
    writer.write_all(&data)?;
    writer.flush()?;
    Ok(())
}

fn frame(reader: impl BufRead) -> Result<Value> {
    let mut line = Vec::new();
    reader.take(1024 * 1024).read_until(b'\n', &mut line)?;
    ensure!(
        line.last() == Some(&b'\n'),
        "incomplete Windows runtime frame"
    );
    Ok(serde_json::from_slice(&line)?)
}

pub(crate) fn runtime() -> Result<()> {
    let request = frame(io::stdin().lock())?;
    let state = Path::new(request["state"].as_str().context("missing runtime state")?);
    ensure!(
        state.is_absolute(),
        "Windows runtime state must be absolute"
    );
    let execution = Execution::new(state, &request)?;
    let job = match Job::new() {
        Ok(job) => job,
        Err(e) => return execution.failure(&e.into()),
    };
    let (kernel, receiver, hello) = match start_kernel(&job, &request) {
        Ok(started) => started,
        Err(error) => {
            job.stop()?;
            if error.is::<process::UnverifiedStageExit>() {
                return Err(error);
            }
            return execution.failure(&error);
        }
    };
    supervise(&execution, &job, kernel, receiver, hello, &request)
}

fn start_kernel(
    job: &Job,
    request: &Value,
) -> Result<(Child, mpsc::Receiver<Result<Value>>, Value)> {
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--kernel")
        .creation_flags(process::NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    controller::strip_credentials(&mut command);
    for (key, value) in request["env"]
        .as_object()
        .context("missing launch environment")?
    {
        ensure!(
            !controller::is_credential(key) || key == controller::WORKER_CALLBACK,
            "controller credential in worker environment"
        );
        command.env(key, value.as_str().context("invalid launch environment")?);
    }
    let mut kernel = command.spawn()?;
    if let Err(error) = job.assign(&kernel) {
        process::stop_staged(&mut kernel)?;
        return Err(error.into());
    }
    let stdout = kernel.stdout.take().context("missing kernel output")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            let reply = frame(&mut stdout);
            let closed = reply.is_err();
            if sender.send(reply).is_err() || closed {
                break;
            }
        }
    });
    let hello = receiver.recv_timeout(LIMIT)??;
    ensure!(
        hello["event"] == "hello" && hello["protocol"] == 1,
        "invalid kernel greeting"
    );
    Ok((kernel, receiver, hello))
}

fn supervise(
    execution: &Execution,
    job: &Job,
    mut kernel: Child,
    receiver: mpsc::Receiver<Result<Value>>,
    hello: Value,
    request: &Value,
) -> Result<()> {
    if execution.read("close.json")?.is_some() {
        job.stop()?;
        return execution.failure(&anyhow::anyhow!("closed before Windows worker launch"));
    }
    let mut input = kernel.stdin.take().context("missing kernel input")?;
    send(
        &mut input,
        &json!({"id":1,"generation":hello["generation"],"unit":execution.ticket["unit"],
        "op":"spawn","cols":120,"rows":40,"cwd":request["cwd"],"argv":request["argv"],"processIdentity":true}),
    )?;
    let spawned = receiver.recv_timeout(LIMIT)??;
    ensure!(
        spawned["id"] == 1 && spawned["generation"] == hello["generation"],
        "invalid kernel spawn response"
    );
    if spawned["ok"] == false {
        job.stop()?;
        return execution.failure(&anyhow::anyhow!("ConPTY worker could not start"));
    }
    ensure!(spawned["ok"] == true, "unconfirmed kernel spawn response");
    let root = &spawned["result"]["identity"];
    let worker = process::open(root)?.context("Windows worker creation identity changed")?;
    ensure!(
        process::exit_code(worker.as_raw_handle())?.is_some()
            || job.contains(worker.as_raw_handle())?,
        "Windows worker is outside the assigned Job"
    );
    let identity = json!({"runtime":process::current_identity()?,
        "kernel":process::identity(kernel.id(), kernel.as_raw_handle())?,"worker":root});
    execution.write(
        "ready.json",
        json!({"result":"accepted","identity":identity,"alive":true,"owned":true}),
    )?;
    // Keep the protocol pipes open for the lifetime of the kernel. It drains
    // the ConPTY continuously, including when no controller is connected.
    let _lifetime = (input, receiver);
    loop {
        let exit = process::exit_code(worker.as_raw_handle())?;
        let close = matches!(execution.read("close.json"), Ok(Some(_)));
        let kernel_exited = kernel.try_wait()?.is_some();
        if close || kernel_exited || (exit.is_some() && job.active()? <= 1) {
            job.stop()?;
            let code = process::exit_code(worker.as_raw_handle())?
                .context("Windows Job drained before its worker signalled")?;
            let (worker, effect) = match (exit, close, kernel_exited) {
                (None, _, true) => (
                    json!({"state":"unknown","exit_code":null,"signal":null,
                    "diagnostic":"ConPTY kernel exited before the worker"}),
                    "runtime-lost",
                ),
                (None, true, false) => (
                    json!({"state":"cancelled","exit_code":code,"signal":null}),
                    "termination-requested",
                ),
                (_, true, _) => (
                    json!({"state":"exited","exit_code":code,"signal":null}),
                    "termination-requested",
                ),
                _ => (
                    json!({"state":"exited","exit_code":code,"signal":null}),
                    "already-exited",
                ),
            };
            return execution.write("finished.json", json!({"result":"completed","identity":identity,
                "worker":worker,"alive":false,"resourceCleanup":"confirmed","jobActiveProcesses":0,"closeEffect":effect}));
        }
        thread::sleep(TICK);
    }
}

pub(super) fn observe(state: &Path, row: &Value) -> Result<(Value, Value)> {
    let unknown = || {
        (
            json!({"state":"unknown"}),
            json!({"result":"uncertain","alive":null}),
        )
    };
    let Ok(execution) = Execution::new(state, row) else {
        return Ok(unknown());
    };
    let observed = (|| -> Result<(Value, Value)> {
        if let Some(finished) = execution.terminal()? {
            execution.verify_launch(row, &finished)?;
            return Ok((finished["worker"].clone(), finished));
        }
        let mut ready = execution
            .read("ready.json")?
            .context("missing Windows launch evidence")?;
        execution.verify_launch(row, &ready)?;
        for field in ["runtime", "kernel"] {
            let process = process::open(&ready["identity"][field])?
                .context("Windows runtime identity changed")?;
            ensure!(
                process::exit_code(process.as_raw_handle())?.is_none(),
                "Windows runtime exited without cleanup evidence"
            );
        }
        let worker = process::open(&ready["identity"]["worker"])?
            .context("Windows worker identity changed")?;
        ready["closePending"] = json!(execution.read("close.json")?.is_some());
        let worker = match process::exit_code(worker.as_raw_handle())? {
            Some(code) => json!({"state":"exited","exit_code":code,"signal":null}),
            None => json!({"state":"running"}),
        };
        Ok((worker, ready))
    })();
    Ok(observed.unwrap_or_else(|_| (json!({"state":"unknown"}), execution.unknown())))
}

pub(super) fn close_blocks_callback(state: &Path, row: &Value) -> Result<bool> {
    let execution = Execution::new(state, row)?;
    Ok(execution.read("close.json")?.is_some())
}

pub(super) fn callback_started(state: &Path, row: &Value) -> Result<bool> {
    let execution = Execution::new(state, row)?;
    // The launch acknowledgement can race a very fast callback. On Windows the
    // callback itself proves execution after the private reservation was bound.
    Ok(execution.dir.is_dir() && execution.read("close.json")?.is_none())
}

pub(crate) fn close_cli(args: &[std::ffi::OsString]) -> Result<Value> {
    let mut id = None;
    let [token] = messages::options(args, ["--owner-token"], Some((&mut id, "dispatch ID")))?;
    let id = id
        .filter(|id| valid_id(id))
        .context("invalid dispatch ID")?;
    let state = writable_state_dir()?;
    let records = state.join("dispatches");
    let selected = assignment::public_record(&records, id)?.context("unknown dispatch")?;
    let scope = record_scope(&selected).context("invalid dispatch scope")?;
    let _scope = controller::lock_scope(&state, scope)?;
    let token = controller::token(token);
    controller::Owners::read(&state)?.require(scope, token.as_deref(), "close")?;
    let current = assignment::public_record(&records, id)?.context("dispatch disappeared")?;
    ensure!(
        current["backend"] == "bundled"
            && current["host"] == "codex"
            && current["publicationDiagnostic"].is_null()
            && assignment::same_publication_identity(&selected, &current),
        "cannot verify Windows close target"
    );
    let execution = Execution::new(&state, &current)?;
    let _attempt = assignment::close_lock(&state, &current)?;
    if let Some(ready) = execution.read("ready.json")? {
        execution.verify_launch(&current, &ready)?;
    } else {
        ensure!(
            current["create"]["result"] != "accepted",
            "Windows launch evidence is missing"
        );
    }
    {
        let _ledger = messages::lock_for_publish(&state)?;
        if execution.read("close.json")?.is_none() && execution.dir.is_dir() {
            execution.write("close.json", json!({"operation":"close"}))?;
        }
    }
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(mut terminal) = execution.terminal()? {
            execution.verify_launch(&current, &terminal)?;
            terminal["result"] = json!("completed");
            return Ok(terminal);
        }
        if Instant::now() >= deadline || !execution.dir.is_dir() {
            let mut unknown = execution.unknown();
            unknown["operation"] = json!("close");
            unknown["dispatch"] = json!(id);
            unknown["round"] = current["round"].clone();
            unknown["attempt"] = current["attempt"].clone();
            return Ok(unknown);
        }
        thread::sleep(TICK);
    }
}
