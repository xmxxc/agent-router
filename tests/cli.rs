use std::{env, fs, path::Path, process::Command};

#[test]
fn version_identifies_the_rust_build() {
    let output = Command::new(env!("CARGO_BIN_EXE_wfc"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let version = String::from_utf8(output.stdout).unwrap();
    assert!(version.starts_with(concat!("wfc ", env!("CARGO_PKG_VERSION"))));
    assert!(version.contains("Rust"));
    assert!(version.contains(env!("WFC_TARGET")));
    assert!(version.contains(env!("WFC_PROFILE")));
    let build_id = version
        .trim_end()
        .strip_suffix(')')
        .and_then(|line| line.split_once(", build "))
        .expect("expected version line ending in a build ID")
        .1;
    assert!(
        !build_id.trim().is_empty(),
        "missing build ID in {version:?}"
    );
    if let Some(expected) = option_env!("WFC_BUILD_ID").filter(|value| !value.trim().is_empty()) {
        assert_eq!(build_id, expected);
    } else {
        assert!(build_id.parse::<u128>().unwrap() > 0);
    }
}

#[test]
fn cli_and_kernel_have_separate_entry_points() {
    for args in [&[][..], &["--help"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_wfc"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .contains("dispatch"));
    }
    for args in [
        ["--kernel", "extra"],
        ["dispatch", "--host"],
        ["unknown", "argument"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_wfc"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn original_kernel_filename_starts_protocol_without_arguments() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("wfc-legacy-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let binary = dir.join(format!("herdr-kernel-prototype{}", env::consts::EXE_SUFFIX));
    fs::hard_link(env!("CARGO_BIN_EXE_wfc"), &binary).unwrap();
    let result = Command::new(&binary).output();
    fs::remove_dir_all(&dir).unwrap();

    let output = result.unwrap();
    assert!(output.status.success());
    let hello: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(hello["event"], "hello");
    assert_eq!(hello["protocol"], 1);
}

#[cfg(windows)]
#[test]
fn windows_pty_async_wait_wakes_the_current_executor() {
    use portable_pty::{
        native_pty_system, win::WinChild, Child, ChildKiller, CommandBuilder, PtySize,
    };
    use std::{
        future::Future,
        pin::Pin,
        sync::{mpsc, Arc},
        task::{Context, Poll, Wake, Waker},
        time::Duration,
    };
    struct Notify(mpsc::Sender<()>);
    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut terminal = pty.master.try_clone_reader().unwrap();
    let drain = std::thread::spawn(move || std::io::copy(&mut terminal, &mut std::io::sink()));
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_wfc"));
    command.arg("--kernel");
    let child: Box<dyn Child> = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let mut child = child.downcast::<WinChild>().unwrap();
    let (old_wake, _old) = mpsc::channel();
    let old = Waker::from(Arc::new(Notify(old_wake)));
    assert!(Pin::new(&mut *child)
        .poll(&mut Context::from_waker(&old))
        .is_pending());

    let (wake, notice) = mpsc::channel();
    let current = Waker::from(Arc::new(Notify(wake)));
    let mut cx = Context::from_waker(&current);
    assert!(Pin::new(&mut *child).poll(&mut cx).is_pending());
    child.clone_killer().kill().unwrap();
    let result = loop {
        match Pin::new(&mut *child).poll(&mut cx) {
            Poll::Ready(v) => break v.unwrap(),
            Poll::Pending => {
                if notice.recv_timeout(Duration::from_secs(10)).is_err() {
                    let status = child.try_wait();
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("process did not wake the waiter, status {status:?}");
                }
            }
        }
    };
    drop(pty.master);
    let _ = drain.join().unwrap();
    assert_eq!(result.exit_code(), 1);
}

#[cfg(target_os = "linux")]
#[test]
fn herdr_reconcile_help_and_missing_evidence() {
    let wfc = env!("CARGO_BIN_EXE_wfc");
    let help = Command::new(wfc)
        .args(["herdr-reconcile", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout)
        .unwrap()
        .contains("--old-writer-evidence"));
    let invalid = Command::new(wfc)
        .args([
            "herdr-reconcile",
            "d-12345678",
            "--pane",
            "p1",
            "--terminal",
            "t1",
            "--actor",
            "operator",
        ])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
}
