//! Start the existing guardian as the main process of one user service.
use super::*;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::{
    fs::{FileTypeExt, MetadataExt},
    net::UnixStream,
};
use store::StartupDecision;

pub(super) const STAGE: &str = "--bundled-user-guardian";
const MANAGER_REQUIRED: &str =
    "bundled dispatch requires the current user's systemd user manager or an explicitly delegated WFC_BUNDLED_CGROUP_ROOT";

fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub(super) fn bus() -> Result<PathBuf> {
    let path = PathBuf::from(format!("/run/user/{}", uid()));
    let valid = fs::symlink_metadata(&path)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == uid() && meta.mode() & 0o077 == 0)
        && fs::symlink_metadata(path.join("bus"))
            .is_ok_and(|meta| meta.file_type().is_socket() && meta.uid() == uid());
    ensure!(valid, "{MANAGER_REQUIRED}");
    Ok(path)
}

fn manager_command(program: &str) -> Result<Command> {
    let runtime = bus()?;
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/bus", runtime.display()),
        )
        .args(["--user", "--no-ask-password"]);
    Ok(command)
}

pub(super) fn context(state: &Path, creator: &Value, deadline: u64) -> Result<Value> {
    let env = |key| -> Result<Option<String>> {
        std::env::var_os(key)
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-UTF-8 {key}"))
            })
            .transpose()
    };
    let cwd = std::env::current_dir()?;
    let exe = fs::metadata("/proc/self/exe")?;
    Ok(json!({
        "state":state.to_str().context("non-UTF-8 native state path")?,
        "cwd":cwd.to_str().context("non-UTF-8 launch directory")?,
        "home":env("HOME")?, "legacyState":env("WFC_STATE")?,
        "creator":creator, "deadline":deadline,
        "exeDevice":exe.dev(), "exeInode":exe.ino()
    }))
}

pub(super) fn restore_legacy_context(context: &Value) -> Result<()> {
    // Called by the single-threaded guardian after authenticating the grant.
    // A relative WFC_STATE must still be interpreted in the caller's directory.
    std::env::set_current_dir(field(context, "cwd")?)?;
    for (key, value) in [
        ("HOME", &context["home"]),
        ("WFC_STATE", &context["legacyState"]),
    ] {
        if let Some(value) = value.as_str() {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }
    Ok(())
}

fn unit_name(ticket: &Value) -> Result<String> {
    let generation = uuid::Uuid::parse_str(field(ticket, "generation")?)?;
    Ok(format!("wfc-bundled-{generation}.service"))
}

pub(super) struct Launcher {
    child: Child,
    grant: UnixStream,
}

impl Drop for Launcher {
    fn drop(&mut self) {
        // The --pipe client waits for the service over D-Bus. Closing this
        // client leaves the already accepted service owned by the user manager.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn start(store: &Store) -> Result<Launcher> {
    let unit = unit_name(&store.ticket)?;
    let parent = std::process::id();
    // Resolve the running ELF, even if its installation path was replaced.
    let exe = format!("/proc/{parent}/exe");
    let (grant, reader) = UnixStream::pair()?;
    grant.set_write_timeout(Some(TIMEOUT))?;
    let mut cmd = manager_command("/usr/bin/systemd-run")?;
    cmd.args(["--quiet", "--pipe", "--collect",
            "--slice=app.slice", "--service-type=exec", "--description=WFC bundled guardian",
            "--property=Delegate=yes", "--property=KillMode=control-group",
            "--property=Restart=no",
            "--property=UnsetEnvironment=WFC_CONTROLLER_TOKEN WFC_OWNER_TOKEN WFC_WORKER_CALLBACK WFC_BUNDLED_CGROUP_ROOT"])
        .arg(format!("--unit={unit}"))
        .arg("--").arg(exe).arg(STAGE)
        .stdin(OwnedFd::from(reader))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Only this auxiliary systemd-run process follows the request process.
    // The actual guardian is spawned independently by the user manager.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context(MANAGER_REQUIRED)?;
    Ok(Launcher { child, grant })
}

pub(super) fn startup_live(context: &Value) -> Result<()> {
    ensure!(
        unix_ms()?
            < context["deadline"]
                .as_u64()
                .context("missing startup deadline")?
            && store::process_state(&context["creator"]) == "alive",
        "bundled startup was interrupted before ready"
    );
    Ok(())
}

fn own_membership(text: &str, uid: u32, unit: &str) -> Result<PathBuf> {
    let expected = format!("/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/{unit}");
    ensure!(
        text.lines()
            .any(|line| line.strip_prefix("0::") == Some(expected.as_str())),
        "bundled guardian is outside its assigned user service cgroup"
    );
    Ok(Path::new("/sys/fs/cgroup").join(expected.trim_start_matches('/')))
}

fn manager_properties(unit: &str) -> Result<String> {
    let mut child = manager_command("/usr/bin/systemctl")?
        .args([
            "show",
            "--no-pager",
            "--property=MainPID,InvocationID,ControlGroup,Delegate,Transient,KillMode,ActiveState",
            "--",
            unit,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + wire::FRAME;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("systemd user manager did not confirm the current invocation");
        }
        thread::sleep(TICK);
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "systemd user manager did not identify this unit"
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn verify_manager_binding(properties: &str, path: &Path, invocation: &str, pid: u32) -> Result<()> {
    let membership = path.strip_prefix("/sys/fs/cgroup")?;
    for expected in [
        format!("MainPID={pid}"),
        format!("InvocationID={invocation}"),
        format!("ControlGroup=/{}", membership.display()),
        "Delegate=yes".to_owned(),
        "Transient=yes".to_owned(),
        "KillMode=control-group".to_owned(),
        "ActiveState=active".to_owned(),
    ] {
        ensure!(
            properties.lines().any(|line| line == expected),
            "systemd user manager binding does not match the guardian"
        );
    }
    Ok(())
}

fn property<'a>(properties: &'a str, name: &str) -> Option<&'a str> {
    properties
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn wait_for_guardian(launcher: &mut Launcher, store: &Store, context: &Value) -> Result<()> {
    let unit = unit_name(&store.ticket)?;
    loop {
        startup_live(context)?;
        ensure!(launcher.child.try_wait()?.is_none(), "{MANAGER_REQUIRED}");
        if let Ok(properties) = manager_properties(&unit) {
            if property(&properties, "ActiveState") == Some("active") {
                let pid: u32 = property(&properties, "MainPID")
                    .context("missing guardian PID")?
                    .parse()?;
                ensure!(pid > 0, "missing guardian PID");
                let invocation =
                    property(&properties, "InvocationID").context("missing systemd invocation")?;
                uuid::Uuid::parse_str(invocation)?;
                let path = own_membership(
                    &fs::read_to_string(format!("/proc/{pid}/cgroup"))?,
                    uid(),
                    &unit,
                )?;
                verify_manager_binding(&properties, &path, invocation, pid)?;
                let exe = fs::metadata(format!("/proc/{pid}/exe"))?;
                ensure!(
                    context["exeDevice"] == exe.dev() && context["exeInode"] == exe.ino(),
                    "bundled guardian is not the executable that issued the grant"
                );
                return startup_live(context);
            }
        }
        thread::sleep(TICK);
    }
}

pub(super) fn guard(state: &Path, spec: &Value, store: Store) -> Result<()> {
    let delegation = (|| -> Result<_> {
        let start = &spec["userService"];
        startup_live(start)?;
        let exe = fs::metadata("/proc/self/exe")?;
        ensure!(
            start["exeDevice"] == exe.dev() && start["exeInode"] == exe.ino(),
            "bundled guardian is not the executable that issued the grant"
        );
        let unit = unit_name(&store.ticket)?;
        let path = own_membership(&fs::read_to_string("/proc/self/cgroup")?, uid(), &unit)?;
        let invocation = std::env::var("INVOCATION_ID").context("missing systemd invocation")?;
        let invocation = uuid::Uuid::parse_str(&invocation)?.simple().to_string();
        verify_manager_binding(
            &manager_properties(&unit)?,
            &path,
            &invocation,
            std::process::id(),
        )?;
        group::verify_service_root(&path)?;
        startup_live(start)?;
        Ok((
            path,
            json!({"unit":unit,"invocation":invocation,"uid":uid()}),
        ))
    })();
    match delegation {
        Ok(root) => runtime::guard(state, spec, store, Some(root)),
        Err(error) => {
            store.save_signed("unstarted", json!({"reason":format!("bundled user service has no verified delegation: {error:#}"),
                "group_cleanup":"not-created"}))?;
            Ok(())
        }
    }
}

fn cancel_unconsumed(state: &Path, store: &Store, spec: &Value, reason: &str) -> Result<()> {
    let Some((current, _lock)) = try_attached_store(state, spec)? else {
        return Ok(());
    };
    ensure_state_writable(state)?;
    if fs::symlink_metadata(current.run.join(STAGE))
        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        && fs::symlink_metadata(current.run.join("unstarted.json"))
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
    {
        // Seal the ticket before a manager that accepted a late start can
        // consume the grant. Both operations use this ticket's unit lock.
        store.save_signed(
            "unstarted",
            json!({"reason":reason,"group_cleanup":"not-created"}),
        )?;
    }
    Ok(())
}

fn proposal_live(prepared: &Value) -> Result<()> {
    ensure!(
        ["guardian", "service", "kernel", "worker"]
            .iter()
            .all(|key| store::process_state(&prepared[*key]) == "alive"),
        "bundled execution stopped before committing ready"
    );
    Ok(())
}

pub(super) fn poll_startup(
    store: &Store,
    context: &Value,
    proposed: &Value,
    may_abort: &mut bool,
) -> Result<Option<StartupDecision>> {
    let result = (|| {
        if let Some(decision) = store.startup_decision(proposed)? {
            if decision == StartupDecision::Ready {
                // Try to seal a link left by the publisher. A sync error
                // cannot invalidate that decision or authorize stopping it.
                let _ = File::open(&store.run).and_then(|dir| dir.sync_all());
            }
            return Ok(Some(decision));
        }
        if *may_abort
            && startup_live(context)
                .and_then(|()| proposal_live(proposed))
                .is_err()
        {
            // Installation may have won even if its directory sync failed.
            return store
                .abort_startup(proposed)
                .or_else(|error| store.startup_decision(proposed)?.ok_or(error))
                .map(Some);
        }
        Ok(None)
    })();
    if result.is_err() {
        // An unreadable decision may have already won. Re-read it without
        // choosing a replacement if its name subsequently goes missing.
        *may_abort = false;
    }
    result
}

pub(super) fn await_ready<G, Check: FnOnce() -> Result<()>>(
    state: &Path,
    store: &Store,
    spec: &Value,
    context: &Value,
    launcher: Result<Launcher>,
    authorize_ready: impl Fn() -> Result<Option<(G, Check)>>,
) -> Result<Value> {
    let ticket = &store.ticket;
    let before_ready = |launcher: &mut Launcher| -> Result<()> {
        wait_for_guardian(launcher, store, context)?;
        store::write_grant(&launcher.grant, STAGE, spec, &store.token)
            .context("could not hand the private grant to the systemd user guardian")?;
        launcher.grant.shutdown(Shutdown::Write)?;
        loop {
            if store.signed("ready").is_ok()
                || store.signed("unstarted").is_ok()
                || store.proof().is_ok()
            {
                return Ok(());
            }
            startup_live(context)?;
            if let Ok(proposed) = store.signed("prepared") {
                if let Some((_authorization, check_owner)) = authorize_ready()? {
                    if let Some((target, _unit_lock)) = try_attached_store(state, spec)? {
                        return target.commit_ready(&proposed, || {
                            ensure_state_writable(state)?;
                            startup_live(context)?;
                            proposal_live(&proposed)?;
                            check_owner()
                        });
                    }
                }
            }
            ensure!(launcher.child.try_wait()?.is_none(), "{MANAGER_REQUIRED}");
            thread::sleep(TICK);
        }
    };
    let failure = match launcher {
        Ok(mut launcher) => before_ready(&mut launcher).err(),
        Err(error) => Some(error),
    };
    if let Some(error) = &failure {
        let _ = cancel_unconsumed(state, store, spec, &format!("{error:#}"));
    }
    if let Ok(identity) = store.signed("ready").and_then(|identity| {
        File::open(&store.run)?.sync_all()?;
        Ok(identity)
    }) {
        return Ok(json!({"result":"accepted","ticket":ticket,"identity":identity}));
    }
    if let Ok(info) = store.signed("unstarted") {
        return Ok(json!({"result":"definitely-not-sent","ticket":ticket,"unstarted":info}));
    }
    if let Ok(proof) = store.proof() {
        if proof["state"] == "startup-aborted" {
            return Ok(
                json!({"result":"uncertain","ticket":ticket,"startup":"aborted",
                "cleanup":"confirmed","proof":proof,"diagnostic":"bundled startup was cancelled before ready"}),
            );
        }
    }
    let mut answer = json!({"result":"uncertain","ticket":ticket,
        "diagnostic":failure.map_or_else(|| "bundled user service exited before ready; inspect the original dispatch".to_owned(),
            |error| format!("bundled user service did not confirm startup: {error:#}"))});
    if let Ok(prepared) = store.signed("prepared") {
        if store.startup_decision(&prepared).ok().flatten() == Some(StartupDecision::Aborted) {
            answer["startup"] = json!("aborted");
            answer["cleanup"] = json!("unknown");
        }
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_service_membership_names_the_exact_user_and_ticket() -> Result<()> {
        let unit = "wfc-bundled-fixture.service";
        let text = format!("0::/user.slice/user-1001.slice/user@1001.service/app.slice/{unit}\n");
        let expected =
            format!("/sys/fs/cgroup/user.slice/user-1001.slice/user@1001.service/app.slice/{unit}");
        assert_eq!(own_membership(&text, 1001, unit)?, Path::new(&expected));
        assert!(own_membership(&text, 1000, unit).is_err());
        assert!(own_membership(&text, 1001, "wfc-bundled-other.service").is_err());
        Ok(())
    }

    #[test]
    fn user_service_startup_rejects_expired_proposals() -> Result<()> {
        let creator = publisher_identity()?;
        startup_live(&json!({"creator":creator,"deadline":unix_ms()? + 60_000}))?;
        assert!(startup_live(&json!({"creator":creator,"deadline":0})).is_err());
        let mut departed = creator.clone();
        departed["start_ticks"] = json!(creator["start_ticks"].as_u64().unwrap() ^ 1);
        assert!(startup_live(&json!({"creator":departed,"deadline":unix_ms()? + 60_000})).is_err());
        Ok(())
    }

    #[test]
    fn user_manager_snapshot_binds_the_grant_recipient() -> Result<()> {
        let name = "wfc-bundled-fixture.service";
        let membership = format!("/user.slice/user-1001.slice/user@1001.service/app.slice/{name}");
        let path = own_membership(&format!("0::{membership}"), 1001, name)?;
        let invocation = "c0ffee2202204ee3beef001122334455";
        let properties = format!("MainPID=72\nInvocationID={invocation}\nControlGroup={membership}\nDelegate=yes\nTransient=yes\nKillMode=control-group\nActiveState=active\n");
        verify_manager_binding(&properties, &path, invocation, 72)?;
        assert!(verify_manager_binding(&properties, &path, invocation, 73).is_err());
        assert!(verify_manager_binding(&properties, &path, "other", 72).is_err());
        Ok(())
    }
}
