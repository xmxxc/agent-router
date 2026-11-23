use super::*;

use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const TOKEN: &str = workflow::controller::WORKER_CALLBACK;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!("wfc-claude-{}", Uuid::new_v4()));
        private_directory(&root).unwrap();
        Self(root)
    }

    fn assignment(&self, attempt: &str) -> Value {
        json!({
            "host": "claude", "backend": "tmux", "dispatch": "d-12345678",
            "round": 1, "attempt": attempt,
            "cwd": self.0, "label": "Claude worker"
        })
    }

    fn publish(&self, row: &mut Value, launch: &mut Launch) {
        let _ledger = workflow::messages::lock_for_publish(&self.0).unwrap();
        workflow::bind_worker_callback(row, Some(launch));
        let dir = self.0.join("dispatches");
        fs::create_dir_all(&dir).unwrap();
        workflow::write_json(&dir.join("d-12345678.json"), row, None).unwrap();
    }

    fn launch(&self) -> Launch {
        Launch {
            argv: vec!["controlled-claude".into()],
            env: [("CLAUDE_CONFIG_DIR".into(), json!(self.0.join("profile")))]
                .into_iter()
                .collect(),
        }
    }
}

fn pending_resume(previous: &Value, next: &mut Value, launch: &mut Launch) {
    workflow::bind_worker_callback(next, Some(launch));
    next["audit"] = json!([{
        "operation": "resume", "result": "pending", "dispatch": previous["dispatch"],
        "fromAttempt": previous["attempt"], "toAttempt": next["attempt"],
        "round": next["round"], "session": previous["hostSession"]
    }]);
}

fn has_adoption(f: &Fixture, row: &Value) -> bool {
    let ledger = workflow::messages::lock_for_publish(&f.0).unwrap();
    adoption(&ledger, row).is_ok()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn argv_value<'a>(launch: &'a Launch, flag: &str) -> &'a str {
    let index = launch.argv.iter().position(|arg| arg == flag).unwrap();
    &launch.argv[index + 1]
}

fn submit(f: &Fixture, record: &Value, launch: &Launch, event: &Value) -> Result<()> {
    record_start(
        &f.0,
        record["dispatch"].as_str().unwrap(),
        record["attempt"].as_str().unwrap(),
        launch.env[TOKEN].as_str().unwrap(),
        serde_json::to_vec(event).unwrap().as_slice(),
    )
}

#[test]
fn resume_requires_adoption_and_persisted_session_and_reuses_original_options() {
    let f = Fixture::new();
    let mut original = f.assignment("a-123456789abc");
    let mut start = f.launch();
    let profile = f.0.join("profile");
    fs::create_dir(&profile).unwrap();
    let alias = f.0.join("profile alias");
    std::os::unix::fs::symlink(&profile, &alias).unwrap();
    start.env.insert("CLAUDE_CONFIG_DIR".into(), json!(alias));
    let mut next = f.assignment("a-000000000001");
    let mut resume = f.launch();
    let reports = f.0.join("reports");
    let dirs = [f.0.clone(), reports.clone()];
    let options = Options {
        model: Some("sonnet"),
        effort: Some("high"),
        ..Options::default()
    };
    prepare_initial(
        &f.0,
        &options,
        &mut original,
        &mut start,
        "first brief".into(),
        &dirs,
    )
    .unwrap();
    f.publish(&mut original, &mut start);
    let id = Uuid::parse_str(argv_value(&start, "--session-id")).unwrap();
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(original["hostSession"], id.to_string());
    assert_eq!(start.argv[1], "first brief");
    assert!(prepare_resume(
        &f.0,
        &original,
        &mut next,
        &mut resume,
        "resume brief".into(),
        &dirs
    )
    .is_err());
    assert_eq!(resume.argv, ["controlled-claude"]);

    let transcript = f.0.join(format!("profile/projects/project/{id}.jsonl"));
    let event = json!({
        "session_id": id.to_string(), "cwd": f.0, "source": "startup",
        "hook_event_name": "SessionStart",
        "transcript_path": alias.join(format!("projects/project/{id}.jsonl")),
        "unused": "do not retain raw hook data"
    });
    for (key, value) in [
        ("session_id", json!(Uuid::new_v4().to_string())),
        ("cwd", json!("/")),
        ("source", json!("resume")),
        ("hook_event_name", json!("UserPromptSubmit")),
        (
            "transcript_path",
            json!(f.0.join(format!("old-profile/projects/p/{id}.jsonl"))),
        ),
    ] {
        let mut wrong = event.clone();
        wrong[key] = value;
        assert!(submit(&f, &original, &start, &wrong).is_err(), "{key}");
    }
    assert!(record_start(
        &f.0,
        "d-12345678",
        "a-000000000001",
        start.env[TOKEN].as_str().unwrap(),
        serde_json::to_vec(&event).unwrap().as_slice()
    )
    .is_err());
    assert!(record_start(
        &f.0,
        "d-12345678",
        "a-123456789abc",
        "wrong-launch-token",
        serde_json::to_vec(&event).unwrap().as_slice()
    )
    .is_err());
    submit(&f, &original, &start, &event).unwrap();
    submit(&f, &original, &start, &event).unwrap();
    assert!(prepare_resume(
        &f.0,
        &original,
        &mut next,
        &mut resume,
        "resume brief".into(),
        &dirs
    )
    .is_err());

    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, b"synthetic session, only its metadata is used").unwrap();
    let mut other_profile = f.launch();
    other_profile
        .env
        .insert("CLAUDE_CONFIG_DIR".into(), json!(f.0.join("other-profile")));
    assert!(prepare_resume(
        &f.0,
        &original,
        &mut next,
        &mut other_profile,
        "resume brief".into(),
        &dirs
    )
    .is_err());
    let proof = prepare_resume(
        &f.0,
        &original,
        &mut next,
        &mut resume,
        "resume brief".into(),
        &dirs,
    )
    .unwrap();
    pending_resume(&original, &mut next, &mut resume);
    {
        let ledger = workflow::messages::lock_for_publish(&f.0).unwrap();
        proof
            .publish(&ledger, &original, &mut next, &resume)
            .unwrap();
    }
    assert_eq!(argv_value(&resume, "--resume"), id.to_string());
    assert_eq!(argv_value(&resume, "--permission-mode"), "auto");
    assert_eq!(argv_value(&resume, "--model"), "sonnet");
    assert_eq!(argv_value(&resume, "--effort"), "high");
    assert_eq!(argv_value(&resume, "--name"), "Claude worker");
    assert_eq!(
        &resume.argv[resume.argv.len() - 2..],
        [f.0.to_str().unwrap(), reports.to_str().unwrap()]
    );
    assert_eq!(resume.argv[1], "resume brief");
    assert!(!resume
        .argv
        .iter()
        .any(|arg| matches!(arg.as_str(), "--session-id" | "--continue")));
    assert_ne!(
        argv_value(&start, "--plugin-dir"),
        argv_value(&resume, "--plugin-dir")
    );
    assert_ne!(start.env[TOKEN], resume.env[TOKEN]);
    assert!(submit(&f, &next, &start, &event).is_err());
    assert!(submit(&f, &original, &start, &event).is_err());
    let mut resumed = event.clone();
    resumed["source"] = json!("resume");
    assert!(submit(&f, &next, &resume, &event).is_err());
    let mut different_project = resumed.clone();
    different_project["transcript_path"] =
        json!(f.0.join(format!("profile/projects/another/{id}.jsonl")));
    assert!(submit(&f, &next, &resume, &different_project).is_err());
    submit(&f, &next, &resume, &resumed).unwrap();
    assert!(has_adoption(&f, &next));
    let path = attempt_dir(&f.0, "d-12345678", "a-000000000001").unwrap();
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    let receipt = path.join("adopted.json");
    assert_eq!(
        receipt.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    let saved = fs::read_to_string(receipt).unwrap();
    assert!(!saved.contains("do not retain raw hook data"));
    assert!(!saved.contains(resume.env[TOKEN].as_str().unwrap()));
}

#[test]
fn conflicting_start_receipts_block_adoption() {
    let f = Fixture::new();
    let mut row = f.assignment("a-123456789abc");
    let mut launch = f.launch();
    prepare_initial(
        &f.0,
        &Options::default(),
        &mut row,
        &mut launch,
        "first brief".into(),
        &[],
    )
    .unwrap();
    f.publish(&mut row, &mut launch);
    let id = argv_value(&launch, "--session-id");
    let mut event = json!({
        "session_id": id, "cwd": f.0, "source": "startup", "hook_event_name": "SessionStart",
        "transcript_path": f.0.join(format!("profile/projects/project/{id}.jsonl"))
    });
    submit(&f, &row, &launch, &event).unwrap();
    assert!(has_adoption(&f, &row));
    event["transcript_path"] = json!(f.0.join(format!("profile/projects/alternate/{id}.jsonl")));
    assert!(submit(&f, &row, &launch, &event).is_err());
    assert!(!has_adoption(&f, &row));
}

struct ResumeRace {
    previous: Value,
    next: Value,
    start: Launch,
    resume: Launch,
    proof: ResumeProof,
    conflict: Value,
}

fn prepared_resume(f: &Fixture) -> ResumeRace {
    let mut previous = f.assignment("a-123456789abc");
    let mut start = f.launch();
    prepare_initial(
        &f.0,
        &Options::default(),
        &mut previous,
        &mut start,
        "first brief".into(),
        &[],
    )
    .unwrap();
    f.publish(&mut previous, &mut start);
    let id = argv_value(&start, "--session-id");
    let original = f.0.join(format!("profile/projects/first/{id}.jsonl"));
    fs::create_dir_all(original.parent().unwrap()).unwrap();
    fs::write(&original, b"synthetic session").unwrap();
    let mut event = json!({
        "session_id": id, "cwd": f.0, "source": "startup", "hook_event_name": "SessionStart",
        "transcript_path": original
    });
    submit(f, &previous, &start, &event).unwrap();
    let mut next = f.assignment("a-000000000001");
    let mut resume = f.launch();
    let proof = prepare_resume(
        &f.0,
        &previous,
        &mut next,
        &mut resume,
        "next brief".into(),
        &[],
    )
    .unwrap();
    pending_resume(&previous, &mut next, &mut resume);
    event["transcript_path"] = json!(f.0.join(format!("profile/projects/other/{id}.jsonl")));
    ResumeRace {
        previous,
        next,
        start,
        resume,
        proof,
        conflict: event,
    }
}

#[test]
fn resume_publication_wins_over_queued_old_hook() {
    struct NotifyAtEof<'a> {
        data: &'a [u8],
        ready: Option<mpsc::Sender<()>>,
    }

    impl Read for NotifyAtEof<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.data.read(buffer)?;
            if count == 0 {
                if let Some(ready) = self.ready.take() {
                    ready.send(()).unwrap();
                }
            }
            Ok(count)
        }
    }

    let f = Fixture::new();
    let ResumeRace {
        previous,
        mut next,
        start,
        resume,
        proof,
        conflict,
    } = prepared_resume(&f);
    let receipt = attempt_dir(&f.0, "d-12345678", "a-123456789abc")
        .unwrap()
        .join("adopted.json");
    let before = fs::read(&receipt).unwrap();
    let conflict = serde_json::to_vec(&conflict).unwrap();
    thread::scope(|scope| {
        let ledger = workflow::messages::lock_for_publish(&f.0).unwrap();
        let (ready, reading) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let state = &f.0;
        let callback = start.env[TOKEN].as_str().unwrap();
        let input = &conflict;
        let pending = scope.spawn(move || {
            finished
                .send(record_start(
                    state,
                    "d-12345678",
                    "a-123456789abc",
                    callback,
                    NotifyAtEof {
                        data: input,
                        ready: Some(ready),
                    },
                ))
                .unwrap();
        });
        reading.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(150)),
            Err(mpsc::RecvTimeoutError::Timeout),
        ));
        proof
            .publish(&ledger, &previous, &mut next, &resume)
            .unwrap();
        drop(ledger);
        let err = result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid Claude attempt capability"),
            "{err}"
        );
        pending.join().unwrap();
    });
    assert_eq!(fs::read(&receipt).unwrap(), before);
    let published = workflow::read_record(&f.0.join("dispatches/d-12345678.json")).unwrap();
    assert_eq!(published["attempt"], next["attempt"]);
    assert_eq!(published["audit"][0]["claudeAdoption"]["source"], "startup");
    let receipt: Adoption = serde_json::from_slice(&before).unwrap();
    assert_eq!(
        published["audit"][0]["claudeAdoption"]["receiptHash"],
        workflow::messages::body_hash(&serde_json::to_string(&receipt).unwrap())
    );
}

#[test]
fn hook_conflict_after_preparation_blocks_resume_publication() {
    let f = Fixture::new();
    let ResumeRace {
        previous,
        mut next,
        start,
        resume,
        proof,
        conflict,
    } = prepared_resume(&f);
    thread::scope(|scope| {
        let state = &f.0;
        let ledger = workflow::messages::lock_for_publish(state).unwrap();
        let (started, waiting) = mpsc::channel();
        let old = &previous;
        let publication = scope.spawn(move || {
            started.send(()).unwrap();
            let ledger = workflow::messages::lock_for_publish(state).unwrap();
            let result = proof.publish(&ledger, old, &mut next, &resume);
            (result, next)
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let err = record_start_locked(
            &ledger,
            "d-12345678",
            "a-123456789abc",
            start.env[TOKEN].as_str().unwrap(),
            serde_json::from_value(conflict).unwrap(),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("conflicting Claude SessionStart receipt"));
        drop(ledger);
        let (result, next) = publication.join().unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Claude session adoption not verified"));
        assert!(next["audit"][0]["claudeAdoption"].is_null());
    });
    assert_eq!(
        workflow::read_record(&f.0.join("dispatches/d-12345678.json")).unwrap(),
        previous
    );
}

#[test]
fn trust_stays_in_effective_worker_home_and_defers_custom_profiles() {
    let f = Fixture::new();
    let router_home = f.0.join("router-home");
    let worker_home = f.0.join("worker-home");
    let profile = worker_home.join("private profile");
    let parent = f.0.join("repo");
    let cwd = parent.join("worker");
    for dir in [&router_home, &profile, &cwd] {
        fs::create_dir_all(dir).unwrap();
    }
    let router_config = router_home.join(".claude.json");
    let worker_config = worker_home.join(".claude.json");
    let custom_config = profile.join(".claude.json");
    let user_hooks = profile.join("settings.json");
    let initial = serde_json::to_vec(&json!({
        "projects": {(parent.to_str().unwrap()): {"hasTrustDialogAccepted": true}}
    }))
    .unwrap();
    let hooks = b"{\"hooks\":{}}\n";
    for config in [&router_config, &worker_config, &custom_config] {
        fs::write(config, &initial).unwrap();
    }
    fs::write(&user_hooks, hooks).unwrap();
    for (alternate, requested) in [
        (Some(profile.as_path()), Some(router_config.as_path())),
        (Some(profile.as_path()), Some(custom_config.as_path())),
        (None, Some(router_config.as_path())),
        (None, Some(Path::new(".claude.json"))),
    ] {
        assert_eq!(
            trust(&cwd, Some(&worker_home), alternate, requested).unwrap(),
            None
        );
    }
    for config in [&router_config, &worker_config, &custom_config] {
        assert_eq!(fs::read(config).unwrap(), initial);
    }
    let same_worker_config = worker_home.join("alias.json");
    std::os::unix::fs::symlink(&worker_config, &same_worker_config).unwrap();
    assert_eq!(
        trust(&cwd, Some(&worker_home), None, Some(&same_worker_config)).unwrap(),
        Some(parent.clone())
    );
    let worker: Value = serde_json::from_slice(&fs::read(&worker_config).unwrap()).unwrap();
    assert_eq!(
        worker["projects"][cwd.to_str().unwrap()]["hasTrustDialogAccepted"],
        true
    );
    fs::remove_file(&worker_config).unwrap();
    std::os::unix::fs::symlink(&router_config, &worker_config).unwrap();
    assert_eq!(trust(&cwd, Some(&worker_home), None, None).unwrap(), None);
    assert_eq!(fs::read(&router_config).unwrap(), initial);
    assert_eq!(fs::read(&custom_config).unwrap(), initial);
    assert_eq!(fs::read(&user_hooks).unwrap(), hooks);
}

#[test]
fn trust_keeps_untrusted_or_invalid_configuration_bytes() {
    let f = Fixture::new();
    let config = f.0.join(".claude.json");
    let ancestor = f.0.join("tree");
    let cwd = f.0.join("tree-other").join("child");
    fs::create_dir_all(&ancestor).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let original = serde_json::to_vec_pretty(&json!({
        "projects": {(ancestor.to_str().unwrap()): {"hasTrustDialogAccepted": true}}
    }))
    .unwrap();
    fs::write(&config, &original).unwrap();
    assert_eq!(trust(&cwd, Some(&f.0), None, Some(&config)).unwrap(), None);
    assert_eq!(fs::read(&config).unwrap(), original);
    fs::write(&config, b"{invalid").unwrap();
    assert!(trust(&cwd, Some(&f.0), None, Some(&config)).is_err());
    assert_eq!(fs::read(&config).unwrap(), b"{invalid");
}

#[test]
fn trust_updates_children_under_lock_and_keeps_mode_and_other_settings() {
    let f = Fixture::new();
    let ancestor = f.0.join("repo");
    let nested = ancestor.join("known");
    let a = nested.join("a");
    let b = nested.join("b");
    for dir in [&a, &b] {
        fs::create_dir_all(dir).unwrap();
    }
    let config = f.0.join(".claude.json");
    let alias = f.0.join("settings link");
    std::os::unix::fs::symlink(&config, &alias).unwrap();
    let original = json!({
        "other": {"setting": 23},
        "projects": {
            (ancestor.to_str().unwrap()): {"hasTrustDialogAccepted": true},
            (nested.to_str().unwrap()): {"hasTrustDialogAccepted": true},
            (a.to_str().unwrap()): {"keep": "project data"}
        }
    });
    fs::write(&config, serde_json::to_vec(&original).unwrap()).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
    let barrier = std::sync::Barrier::new(2);
    thread::scope(|scope| {
        for (path, cwd) in [(&config, &a), (&alias, &b)] {
            let probe = &barrier;
            let nested = &nested;
            let home = &f.0;
            scope.spawn(move || {
                probe.wait();
                assert_eq!(
                    trust(cwd, Some(home), None, Some(path)).unwrap().as_ref(),
                    Some(nested)
                );
            });
        }
    });
    let bytes = fs::read(&config).unwrap();
    let updated: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(updated["other"], original["other"]);
    assert_eq!(
        updated["projects"][a.to_str().unwrap()]["keep"],
        "project data"
    );
    for p in [&a, &b] {
        assert_eq!(
            updated["projects"][p.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }
    assert_eq!(trust(&a, Some(&f.0), None, None).unwrap(), Some(a));
    assert_eq!(fs::read(&config).unwrap(), bytes);
    assert_eq!(
        config.metadata().unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(alias.symlink_metadata().unwrap().is_symlink());
}
