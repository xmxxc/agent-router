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
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsHandle, AsRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;
use winapi::um::winnt::FILE_ATTRIBUTE_REPARSE_POINT;

const LIMIT: Duration = Duration::from_secs(8);
pub(crate) const CLOSE_USAGE: &str = "wfc close DISPATCH [--owner-token TOKEN]";

struct Execution {
    dir: PathBuf,
    ticket: Value,
}

enum WorkerEvidence<'a> {
    Exited { identity: &'a Value, code: u32 },
    LaunchRejected { os_error: Option<i32> },
}

fn valid_launch_identity(identity: &Value) -> bool {
    ["runtime", "kernel", "worker"].into_iter().all(|key| {
        let process = &identity[key];
        process["pid"]
            .as_u64()
            .is_some_and(|pid| (1..=u64::from(u32::MAX)).contains(&pid))
            && process["creation"]
                .as_str()
                .is_some_and(|creation| creation.parse::<u64>().is_ok_and(|time| time != 0))
    })
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid execution directory",
                ));
            }
            Ok(())
        }
        result => result,
    }
}

fn diagnostic(operation: &str, error: &anyhow::Error) -> String {
    let operation = error
        .downcast_ref::<&'static str>()
        .copied()
        .unwrap_or(operation);
    process::diagnostic(operation, error.downcast_ref())
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
            {
                let metadata = file.metadata()?;
                metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            },
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

    fn ensure_parent(&self) -> io::Result<()> {
        ensure_directory(self.dir.parent().unwrap())
    }

    fn close_requested(&self) -> Result<bool> {
        let Some(intent) = self.read("close.json")? else {
            return Ok(false);
        };
        ensure!(
            intent["operation"] == "close",
            "invalid Windows close intent"
        );
        Ok(true)
    }

    fn request_close(&self) -> Result<()> {
        self.ensure_parent()?;
        ensure_directory(&self.dir)?;
        if !self.close_requested()? {
            self.write("close.json", json!({"operation":"close"}))?;
        }
        Ok(())
    }

    fn failed(&self, category: &str, diagnostic: &str) -> Value {
        json!({
            "result":"definitely-not-sent","category":category,"diagnostic":diagnostic,"ticket":self.ticket,
            "alive":false,"resourceCleanup":"confirmed","jobActiveProcesses":0,
            "worker":{"state":"not_started","exit_code":null,"signal":null}
        })
    }

    fn launch_failure(&self, error: &anyhow::Error) -> Value {
        self.failed(
            process::launch_category(error.downcast_ref()),
            &diagnostic("start Windows runtime", error),
        )
    }

    fn cancelled(&self) -> Value {
        self.failed("spawn_failed", "closed before Windows worker launch")
    }

    fn terminal(&self, row: &Value) -> Result<Option<Value>> {
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
            // The published create receipt can corroborate a completed execution
            // even after the startup evidence is lost.
            if row["createPending"] == true || row["create"]["result"] != "accepted" {
                let ready = self
                    .launch_evidence(row)?
                    .context("missing Windows startup evidence")?;
                ensure!(
                    value["identity"] == ready["identity"],
                    "Windows cleanup identity mismatch"
                );
            } else {
                ensure!(
                    valid_launch_identity(&value["identity"]),
                    "invalid Windows cleanup identity"
                );
            }
        }
        self.verify_launch(row, &value)
            .context("verify Windows completion identity")?;
        self.close_fields(&mut value);
        Ok(Some(value))
    }

    fn close_fields(&self, value: &mut Value) {
        value["operation"] = json!("close");
        value["dispatch"] = self.ticket["unit"].clone();
        value["attempt"] = self.ticket["attempt"].clone();
        value["round"] = self.ticket["round"].clone();
        value["closeOperationId"] = json!(format!(
            "close:{}",
            self.ticket["attempt"].as_str().unwrap()
        ));
    }

    fn unknown(&self, diagnostic: impl AsRef<str>) -> Value {
        json!({"result":"uncertain","ticket":self.ticket,"alive":null,"resourceCleanup":"uncertain",
            "category":"confirmation_incomplete","diagnostic":diagnostic.as_ref()})
    }

    fn record_runtime_failure(
        &self,
        error: &anyhow::Error,
        worker: Option<WorkerEvidence<'_>>,
    ) -> Result<()> {
        let mut evidence = self.unknown(diagnostic(
            "Windows runtime could not confirm execution",
            error,
        ));
        match worker {
            Some(WorkerEvidence::Exited { identity, code }) => {
                evidence["identity"] = identity.clone();
                evidence["workerExit"] = json!(code);
            }
            Some(WorkerEvidence::LaunchRejected { os_error }) => {
                evidence["launchRejection"] = json!({"osError":os_error});
            }
            None => (),
        }
        self.write("uncertain.json", evidence)
    }

    fn recorded_uncertainty(&self, row: &Value) -> Result<Option<Value>> {
        let Some(value) = self.read("uncertain.json")? else {
            return Ok(None);
        };
        ensure!(
            value["result"] == "uncertain",
            "invalid Windows runtime failure evidence"
        );
        let reason = value["diagnostic"]
            .as_str()
            .context("missing Windows runtime failure diagnostic")?;
        let mut status = self.unknown(reason);
        status["runtimeResult"] = json!("failed");
        if let Some(code) = value["workerExit"]
            .as_u64()
            .and_then(|code| u32::try_from(code).ok())
        {
            let identity = &value["identity"];
            let matches_launch = if row["create"]["result"] == "accepted" {
                self.verify_launch(row, &value).is_ok()
            } else {
                self.launch_evidence(row)
                    .is_ok_and(|ready| ready.is_some_and(|ready| ready["identity"] == *identity))
            };
            if valid_launch_identity(identity) && matches_launch {
                status["identity"] = identity.clone();
                status["worker"] = json!({"state":"exited","exit_code":code,"signal":null});
            }
        } else if let Some(rejection) = value["launchRejection"]
            .as_object()
            .filter(|_| row["create"]["result"] != "accepted")
        {
            let system = rejection
                .get("osError")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .map(io::Error::from_raw_os_error);
            let cause = process::diagnostic("start ConPTY worker", system.as_ref());
            let category = process::launch_category(system.as_ref());
            status["worker"] = json!({"state":"not_started","exit_code":null,"signal":null});
            status["launchFailure"] = json!({"category":category,"diagnostic":cause});
            status["diagnostic"] = json!(format!("{cause}. {reason}"));
        }
        Ok(Some(status))
    }

    fn verify_launch(&self, row: &Value, evidence: &Value) -> Result<()> {
        if row["create"]["result"] == "accepted" {
            ensure!(
                row["create"]["ticket"] == self.ticket
                    && row["create"]["identity"] == evidence["identity"],
                "Windows launch identity mismatch"
            );
        }
        Ok(())
    }

    fn launch_evidence(&self, row: &Value) -> Result<Option<Value>> {
        let ready = self
            .read("ready.json")
            .context("read Windows launch evidence")?;
        if let Some(ready) = &ready {
            ensure!(
                ready["result"] == "accepted",
                "invalid Windows launch evidence"
            );
            self.verify_launch(row, ready)
                .context("verify Windows launch identity")?;
            ensure!(
                valid_launch_identity(&ready["identity"]),
                "verify Windows launch identity"
            );
        }
        Ok(ready)
    }

    fn completion(&self, row: &Value) -> Result<Option<Value>> {
        if let Some(finished) = self
            .terminal(row)
            .context("read Windows completion evidence")?
        {
            return Ok(Some(finished));
        }
        let saved = &row["create"];
        if saved["ticket"] == self.ticket
            && saved["result"] == "definitely-not-sent"
            && saved["resourceCleanup"] == "confirmed"
            && saved["alive"] == false
            && saved["jobActiveProcesses"] == 0
            && saved["worker"]["state"] == "not_started"
        {
            let mut saved = saved.clone();
            self.close_fields(&mut saved);
            return Ok(Some(saved));
        }
        Ok(None)
    }

    fn outcome(&self, row: &Value) -> Result<Option<Value>> {
        let completion = self.completion(row);
        if matches!(&completion, Ok(Some(_))) {
            return completion;
        }
        match self
            .recorded_uncertainty(row)
            .context("read Windows runtime failure evidence")?
        {
            Some(uncertain) => Ok(Some(uncertain)),
            None => completion,
        }
    }

    fn startup(&self, row: &Value, runtime: &Value) -> Result<Option<Value>> {
        // A fast worker can publish both ready and terminal before create
        // has observed either. Its create outcome is still accepted.
        if let Some(finished) = self.terminal(row)? {
            if finished["result"] == "definitely-not-sent" {
                return Ok(Some(finished));
            }
        }
        let ready = self.read("ready.json")?;
        if let Some(ready) = &ready {
            ensure!(
                ready["result"] == "accepted"
                    && valid_launch_identity(&ready["identity"])
                    && ready["identity"]["runtime"] == *runtime,
                "Windows startup identity mismatch"
            );
        }
        if ready.is_some() {
            return Ok(ready);
        }
        self.recorded_uncertainty(row)
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
    match execution.close_requested() {
        Ok(true) => {
            let cancelled = execution.cancelled();
            // A private write failure must not discard the pending
            // assignment and its already-published close intent.
            let _ = execution.write("finished.json", cancelled.clone());
            return Ok(cancelled);
        }
        Ok(false) => (),
        Err(e) => {
            return Ok(execution.unknown(diagnostic("read Windows close intent before launch", &e)))
        }
    }
    let started = (|| -> Result<_> {
        execution
            .ensure_parent()
            .context("prepare Windows runtime parent")?;
        fs::create_dir(&execution.dir).context("create private Windows execution directory")?;
        let mut command =
            Command::new(env::current_exe().context("resolve Windows runtime program")?);
        controller::strip_credentials(&mut command);
        command
            .arg("--windows-bundled")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        process::background(&mut command)
    })();
    let mut child = match started {
        Ok(child) => child,
        Err(error) => {
            if error.is::<process::UnverifiedStageExit>() {
                return Ok(execution.unknown("Windows staging process exit is unconfirmed"));
            }
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
            {
                return Ok(execution.unknown("Windows execution directory already exists"));
            }
            // The public launch record also preserves this failure if the
            // private execution directory itself could not be created.
            return Ok(execution.launch_failure(&error));
        }
    };
    let exact_program = row["launchConfig"].is_string()
        && launch
            .argv
            .first()
            .is_some_and(|program| Path::new(program).is_absolute());
    let request = json!({"state":state,"dispatch":id,"attempt":row["attempt"],"round":row["round"],
        "windowsExecution":row["windowsExecution"],"cwd":row["cwd"],"argv":launch.argv,
        "env":launch.env,"exactProgram":exact_program});
    let runtime = match process::identity(child.id(), child.as_raw_handle()) {
        Ok(identity) => identity,
        Err(e) => {
            return Ok(execution.unknown(process::diagnostic(
                "verify Windows runtime identity",
                Some(&e),
            )))
        }
    };
    let Some(input) = child.stdin.take() else {
        return Ok(execution.unknown("missing Windows runtime input"));
    };
    if let Err(e) = send_bounded(input, request) {
        return Ok(execution.unknown(diagnostic("send Windows runtime request", &e)));
    }
    drop(ledger);
    drop(scope_lock);
    let deadline = Instant::now() + LIMIT;
    loop {
        match execution.startup(row, &runtime) {
            Ok(None) => (),
            Ok(Some(answer)) => return Ok(answer),
            Err(e) => return Ok(execution.unknown(diagnostic("read Windows startup evidence", &e))),
        }
        if Instant::now() >= deadline || !matches!(child.try_wait(), Ok(None)) {
            // Recheck after the process handle signals. Its last publication
            // can race the first filesystem read in this iteration.
            return Ok(match execution.startup(row, &runtime) {
                Ok(Some(answer)) => answer,
                Ok(None) => execution.unknown("Windows runtime did not confirm launch"),
                Err(e) => execution.unknown(diagnostic("read Windows startup evidence", &e)),
            });
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

fn send_bounded<W: Write + Send + 'static>(mut writer: W, value: Value) -> Result<W> {
    let (written, writing) = mpsc::sync_channel(1);
    thread::Builder::new()
        .spawn(move || {
            let result = send(&mut writer, &value);
            let _ = written.send(result.map(|()| writer));
        })
        .context("start Windows runtime protocol writer")?;
    writing
        .recv_timeout(LIMIT)
        .context("Windows runtime protocol write timed out or disconnected")?
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
    if let Err(error) = run_execution(state, &execution, &request) {
        execution.record_runtime_failure(&error, None)?;
    }
    Ok(())
}

fn run_execution(state: &Path, execution: &Execution, request: &Value) -> Result<()> {
    if execution
        .close_requested()
        .context("read Windows close intent before starting ConPTY")?
    {
        return execution.write("finished.json", execution.cancelled());
    }
    let job = match Job::new_tracked() {
        Ok(job) => job,
        Err(e) => {
            let error = anyhow::Error::from(e).context("create Windows runtime Job");
            return execution.write("finished.json", execution.launch_failure(&error));
        }
    };
    let (kernel, receiver, hello) = match start_kernel(&job, request) {
        Ok(started) => started,
        Err(error) => {
            job.stop_and_verify().context("confirm Windows kernel Job cleanup")?;
            if error.is::<process::UnverifiedStageExit>() {
                return Err(error).context("Windows kernel staging process exit is unconfirmed");
            }
            return execution.write("finished.json", execution.launch_failure(&error));
        }
    };
    supervise(state, execution, job, kernel, receiver, hello, request)
}

fn start_kernel(
    job: &Job,
    request: &Value,
) -> Result<(Child, mpsc::Receiver<Result<Value>>, Value)> {
    let mut command = Command::new(env::current_exe().context("resolve Windows kernel program")?);
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
    let mut kernel = command.spawn().context("start Windows kernel")?;
    if let Err(error) = job.assign(&kernel) {
        process::stop_staged(&mut kernel)?;
        return Err(error).context("assign Windows kernel Job");
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
    let hello = receiver
        .recv_timeout(LIMIT)
        .context("Windows kernel greeting timed out or disconnected")?
        .context("read Windows kernel greeting")?;
    ensure!(
        hello["event"] == "hello" && hello["protocol"] == 1,
        "invalid kernel greeting"
    );
    Ok((kernel, receiver, hello))
}

fn supervise(
    state: &Path,
    execution: &Execution,
    job: Job,
    mut kernel: Child,
    receiver: mpsc::Receiver<Result<Value>>,
    hello: Value,
    request: &Value,
) -> Result<()> {
    let input = kernel.stdin.take().context("missing kernel input")?;
    let launched = {
        // The same ledger serializes close publication and the request that
        // gives the kernel permission to start the worker.
        let _ledger = messages::lock_for_publish(state).context("lock Windows launch boundary")?;
        if execution
            .close_requested()
            .context("read Windows close intent at launch")?
        {
            None
        } else {
            let sent = send_bounded(
                input,
                json!({"id":1,"generation":hello["generation"],"unit":execution.ticket["unit"],
                "op":"spawn","cols":120,"rows":40,"cwd":request["cwd"],"argv":request["argv"],
                "processIdentity":true,"exactProgram":request["exactProgram"]}),
            );
            match sent {
                Ok(input) => Some(input),
                Err(error) => {
                    // An outstanding pipe write can complete after timeout.
                    // Close the Job before releasing the launch boundary.
                    let _ = stop_kernel(&job, &kernel);
                    drop(job);
                    return Err(error).context("send ConPTY worker launch request");
                }
            }
        }
    };
    let Some(input) = launched else {
        stop_kernel(&job, &kernel)?;
        return execution.write("finished.json", execution.cancelled());
    };
    let spawned = receiver
        .recv_timeout(LIMIT)
        .context("ConPTY spawn response timed out or disconnected")?
        .context("read ConPTY spawn response")?;
    ensure!(
        spawned["id"] == 1 && spawned["generation"] == hello["generation"],
        "invalid kernel spawn response"
    );
    if spawned["ok"] == false {
        let os_error = spawned["osError"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok());
        if let Err(error) = stop_kernel(&job, &kernel) {
            if request["exactProgram"] == true {
                return execution.record_runtime_failure(
                    &error,
                    Some(WorkerEvidence::LaunchRejected { os_error }),
                );
            }
            return Err(error);
        }
        let system = os_error.map(io::Error::from_raw_os_error);
        let diagnostic = process::diagnostic("start ConPTY worker", system.as_ref());
        let category = process::launch_category(system.as_ref());
        return execution.write("finished.json", execution.failed(category, &diagnostic));
    }
    ensure!(spawned["ok"] == true, "unconfirmed kernel spawn response");
    let root = &spawned["result"]["identity"];
    let worker = process::open(root)
        .context("open original Windows worker identity")?
        .context("Windows worker creation identity changed")?;
    let exit = process::exit_code(worker.as_raw_handle())?;
    ensure!(
        exit.is_some() || job.contains(worker.as_raw_handle())?,
        "Windows worker is outside the assigned Job"
    );
    job.track(worker.as_handle())?;
    let identity = json!({"runtime":process::current_identity()?,
        "kernel":process::identity(kernel.id(), kernel.as_raw_handle())?,"worker":root});
    execution.write(
        "ready.json",
        json!({"result":"accepted","identity":identity,"alive":true,"owned":true}),
    )?;
    // Keep the protocol pipes open for the lifetime of the kernel. It drains
    // the ConPTY continuously, including when no controller is connected.
    let _lifetime = (input, receiver);
    monitor_worker(execution, &job, &mut kernel, &worker, &identity, exit)
}

fn monitor_worker(
    execution: &Execution,
    job: &Job,
    kernel: &mut Child,
    worker: &OwnedHandle,
    identity: &Value,
    mut exit: Option<u32>,
) -> Result<()> {
    let completed = (|| -> Result<()> {
        loop {
            let close = matches!(execution.close_requested(), Ok(true));
            if exit.is_none() {
                exit = process::exit_code(worker.as_raw_handle())?;
            }
            let kernel_exited = kernel.try_wait()?.is_some();
            if close || kernel_exited || (exit.is_some() && job.active()? <= 1) {
                stop_kernel(job, kernel)?;
                // A worker that exited before the Job snapshot has its own handle.
                let code = process::wait_for_exit(worker.as_raw_handle(), 5000)?
                    .context("Windows worker did not signal after Job cleanup")?;
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
                return execution.write(
                    "finished.json",
                    json!({"result":"completed","identity":identity,"worker":worker,
                    "alive":false,"resourceCleanup":"confirmed","jobActiveProcesses":0,"closeEffect":effect}),
                );
            }
            thread::sleep(TICK);
        }
    })();
    match completed {
        Ok(()) => Ok(()),
        Err(error) => execution.record_runtime_failure(
            &error,
            exit.map(|code| WorkerEvidence::Exited { identity, code }),
        ),
    }
}

fn stop_kernel(job: &Job, kernel: &Child) -> Result<()> {
    job.stop_and_verify().context("confirm Windows kernel Job cleanup")?;
    process::wait_for_exit(kernel.as_raw_handle(), 5000)
        .context("wait for original Windows kernel process")?
        .context("Windows kernel did not signal after Job cleanup")?;
    Ok(())
}

pub(super) fn observe(state: &Path, row: &Value) -> Result<(Value, Value)> {
    let Ok(execution) = Execution::new(state, row) else {
        return Ok((
            json!({"state":"unknown"}),
            json!({"result":"uncertain","alive":null,
            "resourceCleanup":"uncertain","category":"confirmation_incomplete",
            "diagnostic":"invalid Windows execution ticket"}),
        ));
    };
    let (worker, status) = observe_execution(&execution, row);
    Ok((worker, status))
}

fn observe_execution(execution: &Execution, row: &Value) -> (Value, Value) {
    // Read the ticket-bound marker independently of the runtime and its
    // handles, which may vanish before it can publish cleanup.
    let close = execution
        .close_requested()
        .context("read Windows close intent");
    let observed = (|| -> Result<(Value, Value)> {
        if let Some(outcome) = execution.outcome(row)? {
            let worker = outcome
                .get("worker")
                .cloned()
                .unwrap_or_else(|| json!({"state":"unknown"}));
            return Ok((worker, outcome));
        }
        let ready = execution
            .launch_evidence(row)?
            .context("missing Windows launch evidence")?;
        for field in ["runtime", "kernel"] {
            let process = process::open(&ready["identity"][field])
                .context("open Windows runtime or kernel identity")?
                .context("Windows runtime or kernel identity is no longer available")?;
            ensure!(
                process::exit_code(process.as_raw_handle())
                    .context("check Windows runtime exit")?
                    .is_none(),
                "Windows runtime exited without cleanup evidence"
            );
        }
        let worker = process::open(&ready["identity"]["worker"])
            .context("open Windows worker identity")?
            .context("Windows worker identity is no longer available")?;
        let worker = match process::exit_code(worker.as_raw_handle())
            .context("check Windows worker exit")?
        {
            Some(code) => json!({"state":"exited","exit_code":code,"signal":null}),
            None => json!({"state":"running"}),
        };
        Ok((worker, ready))
    })();
    let (worker, mut status) = observed.unwrap_or_else(|error| {
        (
            json!({"state":"unknown"}),
            execution.unknown(diagnostic("could not confirm Windows execution", &error)),
        )
    });
    if status["resourceCleanup"] != "confirmed" {
        match close {
            Ok(pending) => status["closePending"] = json!(pending),
            Err(e) => {
                status = execution.unknown(diagnostic("could not verify Windows close intent", &e));
                return (json!({"state":"unknown"}), status);
            }
        }
    }
    (worker, status)
}

pub(super) fn close_blocks_callback(state: &Path, row: &Value) -> Result<bool> {
    let execution = Execution::new(state, row)?;
    execution.close_requested()
}

pub(super) fn callback_started(state: &Path, row: &Value) -> Result<bool> {
    let execution = Execution::new(state, row)?;
    // The launch acknowledgement can race a very fast callback. On Windows the
    // callback itself proves execution after the private reservation was bound.
    Ok(execution.dir.is_dir() && !execution.close_requested()?)
}

pub(crate) fn close_cli(args: &[std::ffi::OsString]) -> Result<Value> {
    let mut id = None;
    let [token] = messages::options(args, ["--owner-token"], Some((&mut id, "dispatch ID")))?;
    let id = id
        .filter(|id| valid_id(id))
        .context("invalid dispatch ID")?;
    let state = writable_state_dir()?;
    close_dispatch(&state, id, token)
}

fn close_dispatch(state: &Path, id: &str, token: Option<&str>) -> Result<Value> {
    let records = state.join("dispatches");
    let selected = assignment::public_record(&records, id)?.context("unknown dispatch")?;
    let scope = record_scope(&selected).context("invalid dispatch scope")?;
    let _scope = controller::lock_scope(state, scope)?;
    let token = controller::token(token);
    controller::Owners::read(state)?.require(scope, token.as_deref(), "close")?;
    let current = assignment::public_record(&records, id)?.context("dispatch disappeared")?;
    ensure!(
        current["backend"] == "bundled"
            && current["host"] == "codex"
            && current["publicationDiagnostic"].is_null()
            && assignment::same_publication_identity(&selected, &current),
        "cannot verify Windows close target"
    );
    let execution = Execution::new(state, &current)?;
    let _attempt = assignment::close_lock(state, &current)?;
    {
        let _ledger = messages::lock_for_publish(state)?;
        execution.request_close()?;
    }
    let mut status = match execution.launch_evidence(&current) {
        Err(error) => execution
            .terminal(&current)
            .ok()
            .flatten()
            .filter(|finished| finished["result"] == "completed")
            .or_else(|| execution.recorded_uncertainty(&current).ok().flatten())
            .unwrap_or_else(|| {
                execution.unknown(diagnostic(
                    "could not verify Windows launch evidence",
                    &error,
                ))
            }),
        Ok(ready) => {
            let runtime = ready
                .as_ref()
                .and_then(|ready| process::open(&ready["identity"]["runtime"]).ok().flatten());
            let deadline = Instant::now() + LIMIT;
            loop {
                let runtime_alive = runtime.as_ref().is_some_and(|runtime| {
                    matches!(process::exit_code(runtime.as_raw_handle()), Ok(None))
                });
                // The owning runtime publishes its last evidence before its original
                // process handle signals.
                if let Ok(Some(outcome)) = execution.outcome(&current) {
                    break outcome;
                }
                if !runtime_alive || Instant::now() >= deadline {
                    break observe_execution(&execution, &current).1;
                }
                thread::sleep(TICK);
            }
        }
    };
    if matches!(
        status["result"].as_str(),
        Some("completed" | "definitely-not-sent")
    ) {
        status["result"] = json!("completed");
        return Ok(status);
    }
    if status["result"] != "uncertain" {
        status = execution.unknown("Windows close did not confirm Job cleanup");
    }
    if status["worker"].is_null() {
        status["worker"] = json!({"state":"unknown"});
    }
    status["closePending"] = json!(true);
    execution.close_fields(&mut status);
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closes_with_unusable_startup_evidence() -> Result<()> {
        let state = env::temp_dir().join(format!("wfc-close-ready-{}", Uuid::new_v4().simple()));
        let records = state.join("dispatches");
        fs::create_dir_all(&records)?;
        let result = (|| -> Result<()> {
            for (index, diagnostic) in [
                "read Windows launch evidence",
                "read Windows launch evidence",
                "verify Windows launch identity",
            ]
            .into_iter()
            .enumerate()
            {
                let id = format!("d-{:08x}", 80 + index);
                let mut row = json!({"dispatch":id,"attempt":"a-000000000053","round":2,
                    "host":"codex","backend":"bundled","scope":"pending-close",
                    "windowsExecution":Uuid::new_v4().simple().to_string()});
                let execution = Execution::new(&state, &row)?;
                row["create"] = json!({"result":"accepted","ticket":execution.ticket,
                    "identity":{"runtime":process::current_identity()?}});
                assignment::reserve(&records, &id, &row)?;
                execution.ensure_parent()?;
                ensure_directory(&execution.dir)?;
                let ready = execution.dir.join("ready.json");
                match index {
                    0 => fs::create_dir(&ready)?,
                    1 => fs::write(&ready, "{")?,
                    _ => {
                        execution.write("ready.json", json!({"result":"accepted","identity":{}}))?
                    }
                }

                let close = close_dispatch(&state, &id, None)?;
                assert_eq!(close["result"], "uncertain");
                assert_eq!(close["resourceCleanup"], "uncertain");
                assert_eq!(close["alive"], Value::Null);
                assert_eq!(close["worker"]["state"], "unknown");
                assert_eq!(close["closePending"], true);
                assert!(close["diagnostic"]
                    .as_str()
                    .unwrap()
                    .starts_with(diagnostic));
                assert_eq!(close["ticket"], execution.ticket);
                assert!(execution.close_requested()?);
                assert_eq!(close_dispatch(&state, &id, None)?, close);
                let (worker, status) = observe(&state, &row)?;
                assert_eq!(worker["state"], "unknown");
                assert_eq!(status["closePending"], true);
            }
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&state);
        result?;
        cleanup?;
        Ok(())
    }

    #[test]
    fn completed_result_survives_unusable_startup_evidence() -> Result<()> {
        let state = env::temp_dir().join(format!("wfc-finished-{}", Uuid::new_v4().simple()));
        let records = state.join("dispatches");
        fs::create_dir_all(&records)?;
        let result = (|| -> Result<()> {
            let id = "d-00000083";
            let mut row = json!({"dispatch":id,"attempt":"a-000000000083","round":1,
                "host":"codex","backend":"bundled","scope":"finished-readback",
                "windowsExecution":Uuid::new_v4().simple().to_string()});
            let execution = Execution::new(&state, &row)?;
            let identity = json!({
                "runtime":{"pid":101,"creation":"1001"},
                "kernel":{"pid":102,"creation":"1002"},
                "worker":{"pid":103,"creation":"1003"},
            });
            let ready = json!({"ticket":execution.ticket,"result":"accepted",
                "identity":identity,"alive":true,"owned":true});
            let mut accepted = ready.clone();
            accepted["dispatch"] = row["dispatch"].clone();
            accepted["attempt"] = row["attempt"].clone();
            accepted["round"] = row["round"].clone();
            row["backendHandle"] = execution.ticket.clone();
            row["create"] = accepted.clone();
            let published = records.join(format!("{id}.json"));
            write_json(&published, &row, None)?;
            execution.ensure_parent()?;
            ensure_directory(&execution.dir)?;
            let exited = json!({"state":"exited","exit_code":37,"signal":null});
            let finished = json!({"ticket":execution.ticket,"result":"completed",
                "identity":identity,"worker":exited,"alive":false,"resourceCleanup":"confirmed",
                "jobActiveProcesses":0,"closeEffect":"already-exited"});
            let finished_path = execution.dir.join("finished.json");
            write_json(&finished_path, &finished, None)?;

            let check = |result: &str, cleanup: &str, worker: &Value| -> Result<()> {
                let mut record = assignment::public_record(&records, id)?.unwrap();
                let status =
                    crate::workflow::observe_dispatch(&state, &records, &mut record, false)?;
                assert_eq!(record["backendResult"], result);
                assert_eq!(record["worker"], *worker);
                assert_eq!(status["resourceCleanup"], cleanup);
                assert_eq!(
                    status["closeEffect"].as_str(),
                    (result == "completed").then_some("already-exited")
                );
                let close = close_dispatch(&state, id, None)?;
                assert_eq!(close["result"], result);
                assert_eq!(close["worker"], *worker);
                assert_eq!(close["resourceCleanup"], cleanup);
                assert_eq!(close["alive"], status["alive"]);
                assert_eq!(close["closeEffect"], status["closeEffect"]);
                assert_eq!(close["ticket"], execution.ticket);
                assert!(execution.close_requested()?);
                assert_eq!(close_dispatch(&state, id, None)?, close);
                Ok(())
            };

            let ready_path = execution.dir.join("ready.json");
            write_json(&ready_path, &ready, None)?;
            fs::remove_file(&ready_path)?;
            check("completed", "confirmed", &exited)?;
            fs::create_dir(&ready_path)?;
            check("completed", "confirmed", &exited)?;
            fs::remove_dir(&ready_path)?;
            fs::write(&ready_path, "{")?;
            check("completed", "confirmed", &exited)?;
            let mut other_ready = ready.clone();
            other_ready["identity"]["worker"]["creation"] = json!("1004");
            write_json(&ready_path, &other_ready, None)?;
            check("completed", "confirmed", &exited)?;
            fs::remove_file(&ready_path)?;

            let unknown = json!({"state":"unknown"});
            let mut other_worker = finished.clone();
            other_worker["identity"] = other_ready["identity"].clone();
            write_json(&finished_path, &other_worker, None)?;
            check("uncertain", "uncertain", &unknown)?;
            let mut old_finished = finished.clone();
            old_finished["ticket"]["generation"] = json!(Uuid::new_v4().simple().to_string());
            write_json(&finished_path, &old_finished, None)?;
            check("uncertain", "uncertain", &unknown)?;
            let mut incomplete = finished.clone();
            incomplete["jobActiveProcesses"] = json!(1);
            write_json(&finished_path, &incomplete, None)?;
            check("uncertain", "uncertain", &unknown)?;
            write_json(&finished_path, &finished, None)?;
            row.as_object_mut().unwrap().remove("create");
            write_json(&published, &row, None)?;
            write_json(&ready_path, &ready, None)?;
            check("completed", "confirmed", &exited)?;
            fs::remove_file(&ready_path)?;
            check("uncertain", "uncertain", &unknown)?;
            row["create"] = accepted;
            fs::remove_file(&published)?;
            assignment::reserve(&records, id, &row)?;
            check("uncertain", "uncertain", &unknown)?;
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&state);
        result?;
        cleanup?;
        Ok(())
    }

    #[test]
    fn unconfirmed_job_cleanup_preserves_only_an_observed_worker_exit() -> Result<()> {
        let state = env::temp_dir().join(format!("wfc-close-exit-{}", Uuid::new_v4().simple()));
        let records = state.join("dispatches");
        fs::create_dir_all(&records)?;
        let result = (|| -> Result<()> {
            for observed_exit in [true, false] {
                let id = if observed_exit {
                    "d-00000061"
                } else {
                    "d-00000062"
                };
                let mut row = json!({"dispatch":id,"attempt":"a-000000000061","round":1,
                    "host":"codex","backend":"bundled","scope":"worker-exit",
                    "windowsExecution":Uuid::new_v4().simple().to_string()});
                let execution = Execution::new(&state, &row)?;
                let job = Job::new()?; // The fixture deliberately has no Job member watcher.
                let mut kernel = process::tests::fixture("leaf")?;
                job.assign(&kernel)?;
                let mut worker =
                    process::tests::fixture(if observed_exit { "exit" } else { "leaf" })?;
                job.assign(&worker)?;
                let handle = worker.as_handle().try_clone_to_owned()?;
                let identity = json!({"runtime":process::current_identity()?,
                    "kernel":process::identity(kernel.id(), kernel.as_raw_handle())?,
                    "worker":process::identity(worker.id(), worker.as_raw_handle())?});
                row["create"] =
                    json!({"result":"accepted","ticket":execution.ticket,"identity":identity});
                assignment::reserve(&records, id, &row)?;
                execution.request_close()?;
                execution.write(
                    "ready.json",
                    json!({"result":"accepted","identity":identity}),
                )?;

                if observed_exit {
                    drop(worker.stdin.take());
                    assert_eq!(worker.wait()?.code(), Some(37));
                }
                monitor_worker(&execution, &job, &mut kernel, &handle, &identity, None)?;
                assert!(process::exit_code(handle.as_raw_handle())?.is_some());
                kernel.wait()?;
                worker.wait()?;

                let expected = if observed_exit {
                    json!({"state":"exited","exit_code":37,"signal":null})
                } else {
                    json!({"state":"unknown"})
                };
                let (worker, status) = observe(&state, &row)?;
                assert_eq!(worker, expected);
                assert_eq!(status["alive"], Value::Null);
                assert_eq!(status["resourceCleanup"], "uncertain");
                assert_eq!(status["runtimeResult"], "failed");
                assert_eq!(status["jobActiveProcesses"], Value::Null);
                assert!(execution.terminal(&row)?.is_none());
                let close = close_dispatch(&state, id, None)?;
                assert_eq!(close["worker"], expected);
                assert_eq!(close["result"], "uncertain");
                assert_eq!(close["closePending"], true);
                assert_eq!(close["ticket"], execution.ticket);
                assert_eq!(close["diagnostic"], status["diagnostic"]);
                assert_eq!(close_dispatch(&state, id, None)?, close);
                fs::write(execution.dir.join("ready.json"), "{")?;
                assert_eq!(close_dispatch(&state, id, None)?, close);
                assert_eq!(observe(&state, &row)?.0, expected);
            }
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&state);
        result?;
        cleanup?;
        Ok(())
    }

    #[test]
    fn exact_launch_rejection_survives_unconfirmed_cleanup() -> Result<()> {
        let state = env::temp_dir().join(format!("wfc-rejected-{}", Uuid::new_v4().simple()));
        fs::create_dir(&state)?;
        let result = (|| -> Result<()> {
            let row = json!({"dispatch":"d-00000082","attempt":"a-000000000082","round":1,
                "windowsExecution":Uuid::new_v4().simple().to_string()});
            let execution = Execution::new(&state, &row)?;
            execution.ensure_parent()?;
            ensure_directory(&execution.dir)?;
            execution.record_runtime_failure(
                &anyhow::anyhow!("fixture Job evidence unavailable"),
                Some(WorkerEvidence::LaunchRejected { os_error: Some(2) }),
            )?;
            let (worker, status) = observe(&state, &row)?;
            assert_eq!(worker["state"], "not_started");
            assert_eq!(status["result"], "uncertain");
            assert_eq!(status["launchFailure"]["category"], "executable_missing");
            assert!(status["diagnostic"]
                .as_str()
                .unwrap()
                .contains("os error 2"));
            assert_eq!(status["runtimeResult"], "failed");
            assert_eq!(status["resourceCleanup"], "uncertain");
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&state);
        result?;
        cleanup?;
        Ok(())
    }

    #[test]
    fn closes_a_public_reservation_before_its_execution_directory_exists() -> Result<()> {
        let state = env::temp_dir().join(format!("wfc-close-pending-{}", Uuid::new_v4().simple()));
        let records = state.join("dispatches");
        fs::create_dir_all(&records)?;
        let result = (|| -> Result<()> {
            let id = "d-00000051";
            let row = json!({"dispatch":id,"attempt":"a-000000000051","round":2,
                "host":"codex","backend":"bundled","scope":"pending-close",
                "windowsExecution":Uuid::new_v4().simple().to_string()});
            assignment::reserve(&records, id, &row)?;
            let execution = Execution::new(&state, &row)?;
            assert!(!execution.dir.exists());

            let close = close_dispatch(&state, id, None)?;
            assert_eq!(close["result"], "uncertain");
            assert_eq!(close["closePending"], true);
            for field in ["dispatch", "round", "attempt"] {
                assert_eq!(close[field], row[field]);
            }
            assert_eq!(close_dispatch(&state, id, None)?, close);
            let (worker, status) = observe(&state, &row)?;
            assert_eq!(worker["state"], "unknown");
            assert_eq!(status["alive"], Value::Null);
            assert_eq!(status["closePending"], true);
            assert_eq!(status["diagnostic"], "missing Windows launch evidence");

            let mut wrong_attempt = row.clone();
            wrong_attempt["attempt"] = json!("a-000000000052");
            let wrong = Execution::new(&state, &wrong_attempt)?;
            assert!(wrong.close_requested().is_err());
            assert!(run_execution(&state, &wrong, &wrong_attempt).is_err());

            let answer = create(
                &state,
                &row,
                Launch {
                    argv: vec![],
                    env: Default::default(),
                },
                None,
            )?;
            assert_eq!(answer["result"], "definitely-not-sent");
            assert_eq!(answer["worker"]["state"], "not_started");
            assert!(!execution.dir.join("ready.json").exists());
            let done = close_dispatch(&state, id, None)?;
            assert_eq!(done["result"], "completed");
            assert_eq!(done["jobActiveProcesses"], 0);
            assert_eq!(close_dispatch(&state, id, None)?, done);

            // The detached runtime may already have received its request
            // by the time close arrives. It must check before making a Job.
            let second = "d-00000052";
            let mut request = row.clone();
            request["dispatch"] = json!(second);
            request["windowsExecution"] = json!(Uuid::new_v4().simple().to_string());
            assignment::reserve(&records, second, &request)?;
            let second_execution = Execution::new(&state, &request)?;
            assert_eq!(close_dispatch(&state, second, None)?["result"], "uncertain");
            run_execution(&state, &second_execution, &request)?;
            let finished = second_execution.terminal(&request)?.unwrap();
            assert_eq!(finished["worker"]["state"], "not_started");
            assert_eq!(finished["dispatch"], second);
            assert_eq!(close_dispatch(&state, id, None)?, done);
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&state);
        result?;
        cleanup?;
        Ok(())
    }
}
