//! Linux tmux execution seam. The workflow owns attempts, serialization of lifecycle
//! operations, and delivery receipts. Each call can run in a fresh CLI process.
mod binding;
mod history;
mod input;
pub(crate) mod launch;

use anyhow::{ensure, Context, Result};
use history::{CloseEffect, CloseOperation};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Deserialize)]
pub struct Request {
    dispatch: String,
    #[serde(rename = "executionId")]
    execution_id: Option<String>,
    #[serde(rename = "closeOperationId")]
    close_operation_id: Option<String>,
    #[serde(rename = "handoffAudit")]
    handoff_audit: Option<usize>,
    #[serde(rename = "legacyExecutionId")]
    legacy_execution_id: Option<String>,
    #[serde(rename = "legacyWorkerPane")]
    legacy_worker_pane: Option<String>,
    #[serde(rename = "legacyLaunchId")]
    legacy_launch_id: Option<String>,
    #[serde(rename = "legacyRecovery")]
    legacy_recovery: Option<binding::LegacyRecovery>,
    #[serde(default, rename = "freshDispatch")]
    fresh_dispatch: bool,
    backend: Option<String>,
    #[serde(default, rename = "readOnly")]
    read_only: bool,
    #[serde(flatten)]
    op: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Operation {
    Create(launch::Launch),
    Inspect,
    Read,
    Submit {
        text: String,
        host: Option<String>,
        #[serde(default)]
        confirm_reply: bool,
        #[serde(default)]
        confirm_transcript: bool,
    },
    Interrupt,
    Resume {
        #[serde(flatten)]
        launch: launch::Launch,
        host: Option<String>,
        text: Option<String>,
        #[serde(default)]
        probe_read_only: bool,
    },
    Close,
    Revoke,
}

type Reply<T> = std::result::Result<T, Value>;

pub(crate) const APPENDED_USER_ENTRY: &str = "appended-user-entry";

#[derive(Clone, Copy)]
pub(crate) enum InputBoundary {
    PrepareText,
    Text,
    PrepareEnter,
    Enter,
}

fn failure(result: &str, diagnostic: &str) -> Value {
    json!({"result":result, "diagnostic":diagnostic})
}

struct Tmux<'a> {
    read_only: bool,
    protect_offline: bool,
    legacy_execution_id: Option<&'a str>,
    legacy_worker_pane: Option<&'a str>,
    legacy_launch_id: Option<&'a str>,
    legacy_recovery: Option<&'a binding::LegacyRecovery>,
    input_request: bool,
    new_execution_id: Option<&'a str>,
    close_operation_id: Option<&'a str>,
    handoff_audit: Option<usize>,
    close_execution: Option<&'a str>,
    state: &'a Path,
    dispatch: &'a str,
    session: String,
    socket: Option<PathBuf>,
}

const EXCLUSIVE_WINDOW: &str = "#{==:#{window_linked_sessions},1}";

struct Target {
    socket_fd: File,
    worker_pane: Option<String>,
    pane_process: Option<binding::PaneProcess>,
    supervisor: Option<(String, u32)>,
    token: String,
    socket: String,
    server_start: String,
    session: String,
    pane: String,
    server_pid: String,
    owner: String,
    owned: bool,
}

impl Target {
    fn guard_worker_process(&self, condition: &str) -> String {
        let pid = self
            .supervisor
            .as_ref()
            .map(|(_, pid)| *pid)
            .or_else(|| self.pane_process.as_ref().map(|p| p.pid));
        match pid {
            Some(pid) => format!("#{{&&:{condition},#{{==:#{{pane_pid}},{pid}}}}}"),
            None => condition.to_owned(),
        }
    }

    fn worker_pane(&self) -> Reply<&str> {
        self.worker_pane
            .as_deref()
            .filter(|_| {
                self.supervisor.is_some() || self.pane_process.as_ref().is_some_and(|p| p.live())
            })
            .ok_or_else(|| failure("uncertain", "original worker pane is unverified"))
    }
}

fn tmux_arg(value: &str) -> String {
    // tmux's argv parser consumes a final semicolon even after the shell is gone.
    match value.strip_suffix(';') {
        Some(prefix) => format!("{prefix}\\;"),
        None => value.to_owned(),
    }
}

impl Tmux<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        if self.read_only {
            command.arg("-N");
        }
        if let Some(socket) = &self.socket {
            command.arg("-S").arg(socket);
        }
        command
            .arg("-u")
            .env("LC_ALL", "C")
            .env_remove("WFC_STATE")
            .env_remove("WFC_DISPATCH")
            .env_remove("WFC_TMUX_LAUNCH");
        crate::workflow::controller::strip_credentials(&mut command);
        command
    }

    fn run(&self, args: &[&str]) -> Reply<Output> {
        self.run_with_tail(args, &[])
    }

    fn run_with_tail(&self, args: &[&str], tail: &[&str]) -> Reply<Output> {
        // Separate spawn from waiting: only a failed spawn proves tmux did nothing.
        let mut command = self.command();
        command.args(args.iter().map(|s| tmux_arg(s)));
        if !tail.is_empty() {
            command.arg(";").args(tail);
        }
        use std::process::Stdio;
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| failure("definitely-not-sent", "tmux could not run"))?;
        child
            .wait_with_output()
            .map_err(|_| failure("uncertain", "tmux response unavailable"))
    }

    fn target(&self) -> Reply<Option<Target>> {
        let claim = self.claim()?;
        let selector = format!("={}:", self.session);
        let found = self
            .run(&[
                "list-panes",
                "-t",
                &selector,
                "-f",
                "#{pane_active}",
                "-F",
                "#{session_id} #{pane_id} #{pid} #{start_time} :#{WFC_TMUX_LAUNCH}\n#{socket_path}",
            ])
            .map_err(|e| match claim {
                Some(
                    binding::Claim::LegacyPending { .. }
                    | binding::Claim::Observed { .. }
                    | binding::Claim::Launch(_)
                    | binding::Claim::Registered { .. },
                ) if !self.input_request => {
                    failure("uncertain", "could not inspect the original tmux service")
                }
                _ if self.input_request && e["result"] == "definitely-not-sent" => {
                    let mut reply = e;
                    reply["inspectionResult"] = json!("uncertain");
                    reply
                }
                _ => e,
            })?;
        if !found.status.success() {
            let err = String::from_utf8_lossy(&found.stderr);
            let missing_name = err.trim_end() == format!("can't find session: {}", self.session);
            if missing_name
                || err.starts_with("no server running on ")
                || (err.starts_with("error connecting to ")
                    && err.ends_with(" (No such file or directory)\n"))
            {
                self.confirm_absence(claim, missing_name)?;
                return Ok(None);
            }
            return Err(failure("uncertain", "tmux could not inspect the session"));
        }
        let ids = String::from_utf8_lossy(&found.stdout);
        let (ids, socket) = ids
            .strip_suffix('\n')
            .and_then(|s| s.split_once('\n'))
            .filter(|(_, socket)| Path::new(socket).is_absolute())
            .ok_or_else(|| failure("uncertain", "invalid tmux socket identity"))?;
        let fields: Vec<_> = ids.split_whitespace().collect();
        let [session, pane, server_pid, server_start, marked_token] = fields.as_slice() else {
            return Err(failure("uncertain", "invalid tmux target identity"));
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let token = marked_token
            .strip_prefix(':')
            .filter(|s| s.is_empty() || (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())))
            .ok_or_else(|| failure("uncertain", "invalid tmux launch identity"))?;
        if !digits(server_start)
            || !session.strip_prefix('$').is_some_and(digits)
            || !pane.strip_prefix('%').is_some_and(digits)
            || !digits(server_pid)
        {
            return Err(failure("uncertain", "invalid tmux target identity"));
        }
        let socket = self.socket.as_deref().unwrap_or(Path::new(socket));
        let socket_fd = binding::pin_socket(
            socket,
            server_pid
                .parse()
                .map_err(|_| failure("uncertain", "invalid tmux PID"))?,
        )
        .map_err(|_| failure("uncertain", "could not verify tmux socket peer"))?;
        let shown = self.run(&[
            "-S",
            &binding::fd_path(&socket_fd),
            "show-environment",
            "-t",
            session,
            "WFC_STATE",
        ])?;
        if !shown.status.success()
            && String::from_utf8_lossy(&shown.stderr).trim_end() != "unknown variable: WFC_STATE"
        {
            return Err(failure("uncertain", "could not confirm the session owner"));
        }
        let owner = std::str::from_utf8(&shown.stdout)
            .ok()
            .and_then(|s| s.strip_suffix('\n'))
            .and_then(|s| s.strip_prefix("WFC_STATE="))
            .unwrap_or("");
        let owned = shown.status.success()
            && Path::new(owner).is_absolute()
            && fs::canonicalize(owner).is_ok_and(|p| p == self.state);
        let mut t = Target {
            token: token.to_owned(),
            socket: socket.to_string_lossy().into_owned(),
            socket_fd,
            worker_pane: None,
            pane_process: None,
            supervisor: None,
            server_start: server_start.to_string(),
            session: session.to_string(),
            pane: pane.to_string(),
            server_pid: server_pid.to_string(),
            owner: owner.to_owned(),
            owned,
        };
        if owned || (!self.read_only && claim.is_some()) {
            self.verify_claim(claim, &mut t)?;
        }
        Ok(Some(t))
    }

    fn attach_argv(&self, t: &Target) -> Reply<Vec<String>> {
        let binary = env::current_exe()
            .map_err(|_| failure("uncertain", "cannot locate tmux attach helper"))?;
        Ok(vec![
            binary.to_string_lossy().into_owned(),
            "--tmux-attach".into(),
            self.state.to_string_lossy().into_owned(),
            self.dispatch.into(),
            t.socket.clone(),
            t.token.clone(),
        ])
    }

    fn inspect(&self) -> Reply<Value> {
        Ok(match self.target()? {
            None => json!({"result":"accepted","alive":false,"owned":false}),
            Some(t) if t.owned => json!({"result":"accepted","alive":true,"owned":true,
                "session":self.session, "workerBound":t.worker_pane().is_ok(), "attach":self.attach_argv(&t)?}),
            Some(_) => json!({"result":"accepted","alive":true,"owned":false,
                "diagnostic":"session ownership does not match WFC_STATE"}),
        })
    }

    fn owned(&self) -> Reply<Target> {
        match self.target()? {
            Some(t) if t.owned => Ok(t),
            Some(_) => Err(failure(
                "refused",
                "session ownership does not match WFC_STATE",
            )),
            None => Err(failure("definitely-not-sent", "tmux session is absent")),
        }
    }

    // Check the live binding and execute in one server command queue. A stale
    // name must never send a later Enter or kill to a replacement session.
    fn guard(&self, t: &Target, pane: &str, condition: &str) -> String {
        let owner = t
            .owner
            .replace('#', "##")
            .replace(',', "#,")
            .replace('}', "#}");
        let checks = [
            ("pid", t.server_pid.as_str()),
            ("start_time", &t.server_start),
            ("session_id", &t.session),
            ("session_name", &self.session),
            ("pane_id", pane),
            ("WFC_STATE", &owner),
            ("WFC_TMUX_LAUNCH", &t.token),
        ];
        checks
            .into_iter()
            .fold(condition.to_owned(), |rest, (field, value)| {
                format!("#{{&&:#{{==:#{{{field}}},{value}}},{rest}}}")
            })
    }

    fn guarded(&self, t: &Target, pane: &str, action: &str, condition: &str) -> Reply<String> {
        let guard = self.guard(t, pane, condition);
        // A format uses the global environment as a fallback. Require the
        // session-local owner entry to still exist in the same command queue.
        let yes = format!(
            "show-environment -t '{}' WFC_STATE ; {action} ; display-message -p wfc-tmux-confirmed",
            t.session
        );
        let out = self.run(&[
            "-S",
            &binding::fd_path(&t.socket_fd),
            "if-shell",
            "-F",
            "-t",
            pane,
            &guard,
            &yes,
            "display-message -p wfc-tmux-refused",
        ])?;
        if out.status.success() {
            let output = String::from_utf8_lossy(&out.stdout);
            if let Some(data) = output
                .strip_prefix(&format!("WFC_STATE={}\n", t.owner))
                .and_then(|data| data.strip_suffix("wfc-tmux-confirmed\n"))
            {
                return Ok(data.to_owned());
            }
            if output == "wfc-tmux-refused\n" {
                return Err(failure("refused", "tmux session binding changed"));
            }
        }
        Err(failure("uncertain", "tmux did not confirm the operation"))
    }

    fn worker_command(&self, t: &Target, action: &str) -> Reply<String> {
        self.worker_command_if(t, action, "1")
    }

    fn worker_command_if(&self, t: &Target, action: &str, condition: &str) -> Reply<String> {
        if let Some((id, pid)) = &t.supervisor {
            if binding::host_launch_id(*pid, self.state, self.dispatch, t).as_ref() != Some(id) {
                return Err(failure("refused", "original worker is no longer running"));
            }
        }
        let live =
            format!("#{{&&:{EXCLUSIVE_WINDOW},#{{&&:#{{==:#{{pane_dead}},0}},{condition}}}}}");
        self.guarded(t, t.worker_pane()?, action, &t.guard_worker_process(&live))
    }

    fn worker_screen(&self, t: &Target) -> Reply<String> {
        self.worker_command(t, &format!("capture-pane -p -t {}", t.worker_pane()?))
    }

    fn capture(&self, t: &Target, pane: &str) -> Reply<String> {
        self.guarded(
            t,
            pane,
            &format!("capture-pane -p -t {pane}"),
            EXCLUSIVE_WINDOW,
        )
    }

    fn input_target(&self) -> Reply<Target> {
        self.owned()
            .and_then(|t| {
                t.worker_pane()?;
                Ok(t)
            })
            .map_err(|mut e| {
                e["inputResult"] = json!("definitely-not-sent");
                e
            })
    }

    fn replay_unverified_revocation(&self, id: &str) -> Reply<Option<Value>> {
        if !self.was_released(id)? {
            return Ok(None);
        }
        let execution = self.archived_execution(id)?;
        let Some((fact, path)) = self.unverified_released_cancellation(id, execution.as_deref())?
        else {
            return Ok(None);
        };
        let reply = failure(
            "uncertain",
            "worker launch was revoked but tmux session absence is unconfirmed",
        );
        let same_operation = self.close_execution.is_some()
            && self.close_operation_id.is_some()
            && self.close_operation_id == fact["closeOperationId"].as_str();
        Ok(Some(if same_operation {
            revocation_reply(reply, fact, path)
        } else {
            reply
        }))
    }

    fn operate(
        &self,
        op: Operation,
        fresh: bool,
        check_input: &mut dyn FnMut(InputBoundary) -> Reply<()>,
    ) -> Reply<Value> {
        if let Some(id) = self.legacy_execution_id {
            if matches!(
                &op,
                Operation::Submit {
                    confirm_reply: true,
                    ..
                } | Operation::Submit {
                    confirm_transcript: true,
                    ..
                }
            ) && self.close_blocks_input(id).map_err(|_| {
                failure(
                    "definitely-not-sent",
                    "could not verify the execution close state",
                )
            })? {
                return Err(failure(
                    "definitely-not-sent",
                    "original execution is closing or closed",
                ));
            }
            if let Some(reply) = self.replay_unverified_revocation(id)? {
                return Ok(reply);
            }
        }
        if matches!(op, Operation::Revoke) {
            return self
                .revoke_only()?
                .ok_or_else(|| failure("uncertain", "no verified unconsumed reservation"));
        }
        self.check_new_execution()?;
        if fresh {
            self.clear_anonymous_preflight()?;
        }
        if let Some(id) = self.legacy_execution_id {
            if let Some(operation) = self.close_operation_id {
                match self.previous_close(id, operation)? {
                    Some(CloseOperation::Confirmed(effect)) => {
                        let reply = closed(effect);
                        return match self.cancellation_proof(id)? {
                            Some((fact, path)) => {
                                let reply = if matches!(effect, CloseEffect::AlreadyAbsent)
                                    && self.submission(id, self.close_execution).is_err()
                                {
                                    failure("uncertain", "worker launch was revoked but tmux session absence is unconfirmed")
                                } else {
                                    reply
                                };
                                Ok(revocation_reply(reply, fact, path))
                            }
                            None => Ok(reply),
                        };
                    }
                    Some(CloseOperation::Pending) if self.was_released(id)? => {
                        if let Some((fact, path)) = self.cancellation_proof(id)? {
                            if !launch::missing(&self.launch_path(id, "cleanup")) {
                                return Ok(revocation_reply(
                                    failure(
                                        "uncertain",
                                        "retained session cleanup was not confirmed",
                                    ),
                                    fact,
                                    path,
                                ));
                            }
                            self.confirm_cancelled_absence(id, self.close_execution, true)?;
                            self.record_close(CloseOperation::Confirmed(
                                CloseEffect::AlreadyAbsent,
                            ))?;
                            return Ok(revocation_reply(
                                closed(CloseEffect::AlreadyAbsent),
                                fact,
                                path,
                            ));
                        }
                        return Err(failure(
                            "uncertain",
                            "original close action was not confirmed",
                        ));
                    }
                    _ => (),
                }
            }
            if self.was_released(id)? {
                match &op {
                    Operation::Inspect => {
                        return Ok(json!({"result":"accepted","alive":false,"owned":false}))
                    }
                    Operation::Close => {
                        self.record_close(CloseOperation::Confirmed(CloseEffect::AlreadyAbsent))?;
                        return Ok(json!({"result":"completed","closeEffect":"already_absent"}));
                    }
                    Operation::Resume { .. } | Operation::Create(_) if matches!(self.claim()?, Some(binding::Claim::Released(Some(last))) if last == id) => {
                        ()
                    }
                    Operation::Submit { .. } | Operation::Interrupt => {
                        return Ok(
                            json!({"result":"definitely-not-sent","category":"input_not_delivered","diagnostic":"execution has already closed"}),
                        )
                    }
                    _ => return Err(failure("refused", "execution has already closed")),
                }
            }
            if self.read_only {
                if self
                    .claim()?
                    .as_ref()
                    .and_then(binding::Claim::execution_id)
                    != Some(id)
                {
                    return Err(binding::unknown());
                }
            } else {
                self.remember_existing()?;
            }
        }
        match op {
            Operation::Inspect => self.inspect(),
            Operation::Read => {
                let t = self.owned()?;
                Ok(json!({"result":"accepted","screen":self.capture(&t,&t.pane)?}))
            }
            Operation::Interrupt => {
                let target = self.input_target()?;
                self.worker_command(
                    &target,
                    &format!("send-keys -t {} C-c", target.worker_pane()?),
                )?;
                Ok(json!({"result":"accepted"}))
            }
            Operation::Close => self.close(),
            Operation::Revoke => unreachable!("handled before session operations"),
            Operation::Create(launch) => self
                .create(launch)
                .map(|(_, receipt)| json!({"result":"accepted","receipt":receipt})),
            Operation::Submit {
                text,
                host,
                confirm_reply,
                confirm_transcript,
            } => self.input_target().and_then(|target| {
                if confirm_transcript {
                    self.checked_submit(&target, &text, host.as_deref(), check_input)
                } else {
                    self.submit(&target, &text, host.as_deref(), confirm_reply, check_input)
                }
            }),
            Operation::Resume {
                launch,
                host,
                text,
                probe_read_only,
            } => {
                let input = input::ResumeInput::for_host(host.as_deref(), probe_read_only);
                let (target, receipt) = self.create(launch).map_err(|mut e| {
                    if input == Some(input::ResumeInput::SendBrief) {
                        // OpenCode receives this brief only after create returns a verified target.
                        e["inputResult"] = json!("definitely-not-sent");
                    } else if input.is_some() {
                        e["inputResult"] = json!(if e["result"] == "definitely-not-sent"
                            || e["result"] == "refused"
                        {
                            "definitely-not-sent"
                        } else {
                            "uncertain"
                        });
                    }
                    e
                })?;
                let mut result = if let Some(action) = input {
                    let mut observed = self
                        .resume_input(&target, action, text.as_deref())
                        .unwrap_or_else(|e| e);
                    observed["inputResult"] = observed["result"].clone();
                    observed
                } else {
                    json!({"result":"accepted"})
                };
                result["launchResult"] = json!("accepted");
                if result["result"] != "accepted" && result["category"] != "permission_required" {
                    result["result"] = json!("uncertain");
                }
                result["receipt"] = json!(receipt);
                Ok(result)
            }
        }
    }

    fn guarded_close(
        &self,
        t: &Target,
        pane: &str,
        action: &str,
        condition: &str,
    ) -> Reply<String> {
        self.record_close(CloseOperation::Pending)?;
        match self.guarded(t, pane, action, condition) {
            Err(e) if e["result"] == "refused" || e["result"] == "definitely-not-sent" => {
                self.record_close(CloseOperation::NotExecuted)?;
                Err(e)
            }
            outcome => outcome,
        }
    }

    fn finish_cancelled_close(&self, effect: CloseEffect) -> Reply<Value> {
        // Persist the session result before dropping its claim. Session action
        // replay must not turn a confirmed cleanup into another kill.
        self.record_close(CloseOperation::Confirmed(effect))?;
        self.release()?;
        Ok(closed(effect))
    }

    fn revoke_only(&self) -> Reply<Option<Value>> {
        let (Some(id), Some(operation)) = (self.legacy_execution_id, self.close_operation_id)
        else {
            return Ok(None);
        };
        let released = self.was_released(id)?;
        if !released {
            self.revoke_pending_launch()?;
        }
        let Some((fact, path)) = self.cancellation_proof(id)? else {
            return Ok(None);
        };
        let submission = self.submission(id, self.close_execution);
        let reply = match self.previous_close(id, operation)? {
            Some(CloseOperation::Confirmed(effect @ CloseEffect::UnstartedSessionClosed)) => {
                closed(effect)
            }
            Some(CloseOperation::Confirmed(CloseEffect::AlreadyAbsent)) if submission.is_ok() => {
                closed(CloseEffect::AlreadyAbsent)
            }
            _ if submission.is_ok_and(|s| s.phase == launch::SubmissionPhase::Prepared)
                && self
                    .confirm_cancelled_absence(id, self.close_execution, false)
                    .is_ok() =>
            {
                if released {
                    self.record_close(CloseOperation::Confirmed(CloseEffect::AlreadyAbsent))?;
                    closed(CloseEffect::AlreadyAbsent)
                } else {
                    self.finish_cancelled_close(CloseEffect::AlreadyAbsent)?
                }
            }
            _ => failure(
                "uncertain",
                "worker launch was revoked but tmux session absence is unconfirmed",
            ),
        };
        Ok(Some(revocation_reply(reply, fact, path)))
    }

    fn close(&self) -> Reply<Value> {
        self.revoke_pending_launch()?;
        if let Some(claim) = self.claim()? {
            let launch = match claim {
                binding::Claim::Registered { id, execution } => Some((id, Some(execution))),
                binding::Claim::Launch(id) => Some((id, None)),
                _ => None,
            };
            if let Some((id, execution)) = launch {
                let execution = execution.as_deref();
                let fact = if self.legacy_worker_cancelled(&id) {
                    None
                } else {
                    self.cancellation(&id, execution)?
                };
                if let Some((fact, path)) = fact {
                    let keyed_fact = self.close_execution == execution
                        && execution.is_some()
                        && fact["closeOperationId"].as_str() == self.close_operation_id
                        && self.close_operation_id.is_some();
                    let prepared = self
                        .submission(&id, execution)
                        .is_ok_and(|s| s.phase == launch::SubmissionPhase::Prepared);
                    let create_rolled_back = fact["closeOperationId"].is_null();
                    let reply = if prepared
                        && (keyed_fact || create_rolled_back || self.close_execution.is_none())
                    {
                        self.confirm_cancelled_absence(&id, execution, false)
                            .and_then(|()| self.finish_cancelled_close(CloseEffect::AlreadyAbsent))
                    } else if self.close_execution.is_some() && !keyed_fact {
                        Err(binding::unknown())
                    } else {
                        match self.target() {
                            Ok(None) if !launch::missing(&self.launch_path(&id, "cleanup")) => Err(
                                failure("uncertain", "retained session cleanup was not confirmed"),
                            ),
                            Ok(None) => self.finish_cancelled_close(CloseEffect::AlreadyAbsent),
                            Ok(Some(target)) => {
                                self.close_cancelled_session(&id, &target).and_then(|()| {
                                    self.finish_cancelled_close(CloseEffect::UnstartedSessionClosed)
                                })
                            }
                            Err(error) => Err(error),
                        }
                    };
                    return if keyed_fact {
                        Ok(revocation_reply(
                            reply.unwrap_or_else(|error| error),
                            fact,
                            path,
                        ))
                    } else {
                        reply
                    };
                }
            }
        }
        let effect = if let Some(t) = self.target()? {
            if !t.owned {
                return Err(failure(
                    "refused",
                    "session ownership does not match WFC_STATE",
                ));
            }
            let exclusive = "#{m/r:^1+$,#{W:#{?#{==:#{window_linked_sessions},1},1,0}}}";
            let kill = format!("kill-session -t '{}'", t.session);
            let worker_pane = t.worker_pane.as_deref().ok_or_else(binding::unknown)?;
            let worker = self.close_worker(&t)?;
            if worker == binding::Worker::Exited {
                // Once absent from the pinned server, a pane ID cannot move back.
                if self.worker_pane_absent(&t)? {
                    self.guarded_close(&t, &t.pane, &kill, exclusive)?;
                } else {
                    let dead_pane = format!("#{{&&:{exclusive},#{{pane_dead}}}}");
                    self.guarded_close(&t, worker_pane, &kill, &dead_pane)?;
                }
                CloseEffect::WorkerExited
            } else {
                worker.check_before_kill()?;
                t.worker_pane()?;
                let hangup_target = self.close_hangup_target(&t, &worker)?;
                // Leave a retained dead pane intact for reconciliation.
                let action = format!(
                    "if-shell -F -t {} '#{{pane_dead}}' {{ display-message -p dead }} {{ {kill} }}",
                    worker_pane
                );
                let outcome = self.guarded_close(
                    &t,
                    worker_pane,
                    &action,
                    &t.guard_worker_process(exclusive),
                )?;
                if outcome == "dead\n" {
                    self.record_close(CloseOperation::NotExecuted)?;
                }
                if !outcome.is_empty() {
                    return Err(failure("uncertain", "worker pane is no longer active"));
                }
                if self.close_worker(&t)? != worker {
                    return Err(failure(
                        "uncertain",
                        "session closed but worker exit order is unconfirmed",
                    ));
                }
                // The pane's supervisor survives hangup to write exit and tree-stop
                // evidence. Deliver the confirmed close's hangup to its verified child.
                if let Some(target) = hangup_target {
                    if let Err(error) = binding::signal_worker(&target, libc::SIGHUP) {
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            return Err(failure(
                                "uncertain",
                                "session closed but original worker hangup is unconfirmed",
                            ));
                        }
                    }
                }
                CloseEffect::SessionClosed
            }
        } else {
            if let (Some(id), Some(operation)) = (self.legacy_execution_id, self.close_operation_id)
            {
                if matches!(
                    self.previous_close(id, operation)?,
                    Some(CloseOperation::Pending)
                ) {
                    return Err(failure(
                        "uncertain",
                        "original close action was not confirmed",
                    ));
                }
            }
            CloseEffect::AlreadyAbsent
        };
        self.record_close(CloseOperation::Confirmed(effect))?;
        self.release()?;
        Ok(json!({"result":"completed", "closeEffect":effect}))
    }
}

fn closed(effect: CloseEffect) -> Value {
    json!({"result":"completed", "closeEffect":effect})
}

fn revocation_reply(mut reply: Value, fact: Value, path: PathBuf) -> Value {
    for field in [
        "category",
        "dispatch",
        "executionId",
        "closeOperationId",
        "launchId",
    ] {
        reply[field] = fact[field].clone();
    }
    reply["cancellationEvidence"] = json!(path);
    reply
}

fn valid_dispatch(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub(crate) use binding::record_created_session;

pub(crate) fn attach(state: &Path, dispatch: &str, socket: &Path, token: &str) -> Result<()> {
    ensure!(
        valid_dispatch(dispatch)
            && token.len() == 32
            && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid attach identity"
    );
    let state = state.canonicalize()?;
    let backend = Tmux {
        state: &state,
        dispatch,
        read_only: false,
        protect_offline: true,
        legacy_execution_id: None,
        legacy_worker_pane: None,
        legacy_launch_id: None,
        legacy_recovery: None,
        input_request: false,
        new_execution_id: None,
        close_operation_id: None,
        handoff_audit: None,
        close_execution: None,
        session: format!("wfc-{dispatch}"),
        socket: Some(socket.to_owned()),
    };
    let lock = backend
        .lock_dispatch()
        .map_err(|_| anyhow::anyhow!("cannot verify tmux attach claim"))?;
    ensure!(
        matches!(
            backend.claim(),
            Ok(Some(
                binding::Claim::Launch(_)
                    | binding::Claim::Registered { .. }
                    | binding::Claim::Observed { .. }
            ))
        ),
        "tmux attach claim expired"
    );
    let t = backend
        .owned()
        .map_err(|_| anyhow::anyhow!("tmux attach target is unavailable"))?;
    ensure!(t.token == token, "tmux attach target changed");
    drop(lock);
    // Keep the verified socket inode pinned for the client. Recheck the
    // session, pane and explicit owner in the server queue at actual attach.
    let attach = format!(
        "show-environment -t '{0}' WFC_STATE ; attach-session -E -t '{0}'",
        t.session
    );
    let condition = backend.guard(&t, &t.pane, EXCLUSIVE_WINDOW);
    let status = backend.command()
        .args(["-N", "-S", &binding::fd_path(&t.socket_fd), "if-shell", "-F", "-t"])
        .arg(format!("={}:", backend.session))
        .arg(condition)
        .arg(attach)
        .arg("display-message -p 'tmux attach refused: session binding changed' ; run-shell 'exit 1'")
        .status()?;
    ensure!(status.success(), "tmux attach refused or disconnected");
    Ok(())
}

fn write_refusal(state: &Path, read_only: bool) -> Option<Value> {
    if read_only {
        return None;
    }
    crate::workflow::ensure_state_writable(state).err().map(|error| {
        json!({"result":"refused","category":"legacy_state_write_blocked","diagnostic":format!("{error:#}")})
    })
}

pub fn execute(state: &Path, request: Request) -> Result<Value> {
    execute_checked(state, request, None, &mut |_| Ok(()))
}

pub(crate) fn execute_checked(
    state: &Path,
    request: Request,
    socket: Option<&str>,
    check_input: &mut dyn FnMut(InputBoundary) -> std::result::Result<(), Value>,
) -> Result<Value> {
    ensure!(
        matches!(request.backend.as_deref(), None | Some("" | "tmux")),
        "request selects another execution backend"
    );
    ensure!(valid_dispatch(&request.dispatch), "invalid dispatch ID");
    ensure!(
        request
            .legacy_execution_id
            .as_deref()
            .is_none_or(|id| !id.is_empty()),
        "empty legacy execution ID"
    );
    ensure!(
        request.legacy_worker_pane.is_none() || request.legacy_execution_id.is_some(),
        "legacy pane requires an execution ID"
    );
    ensure!(
        request
            .legacy_worker_pane
            .as_deref()
            .is_none_or(binding::valid_pane),
        "invalid legacy worker pane"
    );
    ensure!(
        !request.fresh_dispatch
            || (request.legacy_execution_id.is_none()
                && matches!(request.op, Operation::Create(_) | Operation::Inspect)),
        "fresh dispatch cannot select a legacy execution"
    );
    ensure!(
        request
            .legacy_launch_id
            .as_deref()
            .is_none_or(|id| request.legacy_execution_id.is_some() && binding::valid_launch_id(id)),
        "invalid legacy launch ID"
    );
    ensure!(
        !request.read_only
            || (matches!(request.op, Operation::Inspect)
                && !request.fresh_dispatch
                && request.legacy_recovery.is_none()),
        "readOnly requires ordinary inspect without legacy recovery"
    );
    if let Some(refusal) = write_refusal(state, request.read_only) {
        return Ok(refusal);
    }
    if matches!(request.op, Operation::Create(_) | Operation::Resume { .. }) {
        fs::create_dir_all(state).context("create tmux state directory")?;
    }
    let state = if state.exists() {
        state.canonicalize()?
    } else {
        std::path::absolute(state)?
    };
    let launching = matches!(&request.op, Operation::Create(_) | Operation::Resume { .. });
    ensure!(request.execution_id.as_deref().is_none_or(|id| !id.is_empty() && (launching || request.legacy_execution_id.is_none())), "invalid or competing execution selector");
    ensure!(
        request
            .close_operation_id
            .as_deref()
            .is_none_or(|id| !id.is_empty()
                && matches!(request.op, Operation::Close | Operation::Revoke)
                && (request.execution_id.is_some() || request.legacy_execution_id.is_some())),
        "close operation requires an execution selector"
    );
    ensure!(
        !matches!(request.op, Operation::Revoke)
            || (request.execution_id.is_some() && request.close_operation_id.is_some()),
        "revoke requires the native execution and close operation IDs"
    );
    ensure!(
        request.legacy_recovery.is_none()
            || (matches!(request.op, Operation::Inspect) && !request.fresh_dispatch),
        "legacyRecovery requires ordinary inspect"
    );
    let native_id;
    let mut backend = Tmux {
        state: &state,
        dispatch: &request.dispatch,
        read_only: request.read_only,
        protect_offline: !request.fresh_dispatch && !matches!(&request.op, Operation::Create(_)),
        legacy_execution_id: request.legacy_execution_id.as_deref(),
        legacy_worker_pane: request.legacy_worker_pane.as_deref(),
        legacy_launch_id: request.legacy_launch_id.as_deref(),
        legacy_recovery: request.legacy_recovery.as_ref(),
        new_execution_id: request.execution_id.as_deref().filter(|_| launching),
        close_operation_id: request.close_operation_id.as_deref(),
        handoff_audit: request.handoff_audit,
        close_execution: request
            .execution_id
            .as_deref()
            .filter(|_| request.close_operation_id.is_some()),
        input_request: matches!(request.op, Operation::Submit { .. } | Operation::Interrupt),
        session: format!("wfc-{}", request.dispatch),
        socket: socket
            .map(std::ffi::OsString::from)
            .or_else(|| env::var_os("WFC_TMUX_SOCKET"))
            .map(std::path::absolute)
            .transpose()?,
    };
    let _lock = match backend.lock_dispatch() {
        Ok(lock) => lock,
        Err(result) => return Ok(result),
    };
    if let Some(refusal) = write_refusal(&state, request.read_only) {
        return Ok(refusal);
    }
    if let Some(audit) = request.handoff_audit {
        ensure!(
            matches!(&request.op, Operation::Create(_)) && !request.fresh_dispatch,
            "handoff audit requires a replacement create"
        );
        let attempt = request
            .execution_id
            .as_deref()
            .context("missing handoff execution")?;
        if let Err(error) =
            crate::workflow::handoff::require_launch(&state, &request.dispatch, attempt, audit)
        {
            return Ok(failure("definitely-not-sent", &format!("{error:#}")));
        }
    }
    let inspect = matches!(request.op, Operation::Inspect);
    if let Some(execution) = request.execution_id.as_deref().filter(|_| !launching) {
        native_id = match backend.registered_launch(execution) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        backend.legacy_execution_id = Some(&native_id);
    }
    if matches!(&request.op, Operation::Close) {
        if let Err(error) = backend.require_close_worker_tree(request.execution_id.as_deref()) {
            return Ok(error);
        }
    }
    let mut result = backend
        .operate(request.op, request.fresh_dispatch, check_input)
        .unwrap_or_else(|mut error| {
            if backend.input_request && error["result"] == "definitely-not-sent" {
                error["category"] = json!("input_not_delivered");
            }
            error
        });
    if inspect {
        if request.read_only && result["alive"] == true && result["owned"] == false {
            return Ok(result);
        }
        let accepted = result["result"] == "accepted";
        let launch = if let Some(execution) = &request.execution_id {
            let id = backend
                .legacy_execution_id
                .expect("resolved execution selector");
            if accepted
                && result["alive"] == false
                && backend.legacy_worker_cancelled(id)
                && backend
                    .submission(id, Some(execution))
                    .is_ok_and(|row| row.phase == launch::SubmissionPhase::LegacyAbsent)
            {
                return Ok(result);
            }
            match backend.confirm_launch(id) {
                Ok(worker) if accepted || worker == binding::Worker::Exited => {
                    result["executionId"] = json!(execution);
                    Some(id.to_owned())
                }
                Err(e) if accepted => return Ok(e),
                _ => None,
            }
        } else if let Some(execution) = &request.legacy_execution_id {
            match backend.observed_launch(execution, !accepted) {
                Ok(Some(id)) => {
                    result["legacyExecutionId"] = json!(execution);
                    Some(id)
                }
                Ok(None) => None,
                Err(e) => return Ok(e),
            }
        } else {
            None
        };
        if let Some(id) = launch {
            result["launchId"] = json!(id);
            result["receipt"] = json!(state.join("launches").join(format!("{id}.json")));
        }
    }
    if let Some(op) = &request.close_operation_id {
        if result["result"] == "completed" {
            result["closeOperationId"] = json!(op);
        }
    }
    Ok(result)
}
