use super::*;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bundled_first_task_resume_requires_original_stop_and_starts_once() -> Result<()> {
    for case in [
        "accepted",
        "uncertain",
        "missing-ready",
        "no-stop-receipt",
        "wrong-ticket",
        "wrong-session",
        "missing-home",
        "changed-task",
        "unresolved-source",
        "unresolved-reply",
        "completed",
        "wrong-owner",
        "legacy-fence",
    ] {
        let fixture = Fixture(std::env::temp_dir().join(format!("wfc-resume-{}", Uuid::new_v4())));
        let dir = fixture.0.join("dispatches");
        fs::create_dir_all(&dir)?;
        let state = fixture.0.canonicalize()?;
        let dir = state.join("dispatches");
        let cwd = state.join("work");
        let home = state.join("codex");
        let sessions = home.join("sessions");
        fs::create_dir(&cwd)?;
        fs::create_dir_all(&sessions)?;
        let task = cwd.join("original-task.md");
        let report = cwd.join("original-report.md");
        fs::write(&task, "ordinary original task\n")?;
        fs::write(&report, "ordinary preserved report\n")?;
        let dispatch = "d-1234abcd";
        let attempt = "a-0123456789ab";
        let scope = "resume-fixture";
        let owner = "ordinary synthetic owner";
        let managed = std::env::var_os("WFC_BUNDLED_CGROUP_ROOT").is_none();
        let (ticket, cleanup, _) = crate::bundled::resume_ticket_fixture(
            &state,
            dispatch,
            attempt,
            case != "no-stop-receipt",
            managed,
        )?;
        let mut recorded_ticket = ticket.clone();
        if case == "wrong-ticket" {
            recorded_ticket["nonce"] = json!(Uuid::new_v4().to_string());
        }

        let config = state.join("launch.json");
        // The real Codex host preparation reads only this synthetic home.
        super::super::write_json(&config, &json!({"env":{"CODEX_HOME":home}}), None)?;
        let session = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp() as f64;
        super::super::write_json(
            &state.join("controller-owners.json"),
            &json!({(scope):{"owner":"fixture","generation":7,"until":now + 600.0,
                "fingerprint":messages::body_hash(owner),"retired":[]}}),
            None,
        )?;
        let message = if case == "completed" {
            json!({"type":"done","report":report})
        } else {
            json!({"type":"question","question":"ordinary archived question"})
        };
        let mut archived = message;
        archived["dispatch"] = json!(dispatch);
        archived["round"] = json!(1);
        archived["attempt"] = json!(attempt);
        archived["messageId"] = json!("m-original");
        archived["id"] = json!(17);
        let mut row = json!({"dispatch":dispatch,"attempt":attempt,"round":1,"scope":scope,
            "role":"worker","host":"codex","backend":"bundled","cwd":cwd,"task":task,"report":report,
            "launchConfig":config,"worker":{"state":"running"},"created":now,"codexHome":home,
            "messages":[archived],"hostSession":session,"bundledStartupCommit":managed,
            "backendHandle":recorded_ticket,"create":{"result":"accepted","dispatch":dispatch,
                "round":1,"attempt":attempt,"ticket":recorded_ticket}});
        let opening = host::worker_brief(&row, host::FIRST_BRIEF)?;
        row["codexBriefHash"] = json!(messages::body_hash(&opening));
        if case == "missing-home" {
            row.as_object_mut()
                .context("fixture row")?
                .remove("codexHome");
        }
        if case == "changed-task" {
            row["task"] = json!(cwd.join("other-task.md"));
        }
        if case == "unresolved-source" {
            row["audit"] = json!([{"id":8,"operation":"nudge","result":"pending",
                "attempt":attempt,"round":1}]);
        }
        let first_message = if case == "wrong-session" {
            "another task"
        } else {
            &opening
        };
        fs::write(
            sessions.join(format!("rollout-{session}.jsonl")),
            format!(
                "{}\n{}\n",
                json!({"type":"session_meta","payload":{"id":session,"cwd":cwd,"source":"cli"}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":first_message}})
            ),
        )?;
        let path = dir.join(format!("{dispatch}.json"));
        super::super::write_json(&path, &row, None)?;
        let before = fs::read(&path)?;
        let close_path = dir.join(format!("{dispatch}.{attempt}.closing"));
        if case != "wrong-ticket" {
            super::super::write_json(
                &close_path,
                &json!({"ticket":ticket,"round":1,"workerOnClose":null}),
                None,
            )?;
        }
        let close_before = fs::read(&close_path).ok();
        let signed_before = fs::read(&cleanup).ok();
        let ledger = state.join("messages.json");
        if case == "unresolved-reply" {
            messages::stage_legacy_messages(
                &state,
                &[json!({
                    "id":18,"messageId":"m-reply","dispatch":dispatch,"attempt":attempt,"round":1,
                    "role":"worker","scope":scope,"state":"received","statusVersion":2,
                "reply_payload":{"to":17,"questionMessageId":"m-original",
                    "answer":"ordinary reply","inputResult":"pending"}
                })],
            )?;
        }
        let ledger_before = fs::read(&ledger).ok();
        if case == "legacy-fence" {
            fs::write(state.join("router.db-journal"), b"synthetic legacy fence")?;
        }
        let args = [
            dispatch,
            "--owner-token",
            if case == "wrong-owner" {
                "wrong"
            } else {
                owner
            },
        ]
        .map(OsString::from);
        let mut starts = 0;
        let result =
            Resume::parse(&args)?.run_in(state.clone(), |state, next, prior, launch, token| {
                starts += 1;
                assert!(matches!(case, "accepted" | "uncertain" | "missing-ready"));
                assert_eq!(token, Some(owner));
                assert_eq!(prior["ticket"], ticket);
                assert_eq!(prior["proof"]["proves"], "execution-empty");
                assert_eq!(next["round"], 1);
                assert_ne!(next["attempt"], attempt);
                assert_eq!(next["messages"], row["messages"]);
                assert_eq!(next["task"], row["task"]);
                assert_eq!(next["report"], row["report"]);
                assert_eq!(next["audit"][0]["result"], "pending");
                assert_eq!(super::super::read_record(&path)?, *next);
                assert_eq!(launch.env.get("WFC_ATTEMPT"), Some(&next["attempt"]));
                assert_eq!(launch.env.get("CODEX_HOME"), Some(&row["codexHome"]));
                assert_eq!(launch.argv[1], "resume");
                assert_eq!(launch.argv[launch.argv.len() - 2], session);
                crate::bundled::apply_resume_cleanup(state, &ticket, &prior["proof"])?;
                if case == "uncertain" {
                    anyhow::bail!("ordinary synthetic ambiguous launch");
                }
                if case == "missing-ready" {
                    let mut replacement = ticket.clone();
                    replacement["attempt"] = next["attempt"].clone();
                    return Ok(json!({"result":"accepted","ticket":replacement}));
                }
                let (replacement, _, ready) = crate::bundled::resume_ticket_fixture(
                    state,
                    dispatch,
                    next["attempt"].as_str().context("replacement attempt")?,
                    false,
                    managed,
                )?;
                Ok(json!({"result":"accepted","ticket":replacement,"identity":ready}))
            });
        if matches!(case, "accepted" | "uncertain" | "missing-ready") {
            let result = result?;
            let outcome = if case == "accepted" {
                "accepted"
            } else {
                "uncertain"
            };
            assert_eq!(starts, 1);
            assert_eq!(result["result"], outcome);
            assert_eq!(result["audit"], 1);
            let settled = super::super::read_record(&path)?;
            assert_eq!(settled["audit"][0]["result"], outcome);
            assert_eq!(settled["audit"][0]["priorBackendHandle"], ticket);
            assert_eq!(settled["audit"][0]["priorCreate"], row["create"]);
            assert_eq!(settled["rounds"][0]["attempt"], attempt);
            assert_eq!(settled["messages"], row["messages"]);
            if case == "accepted" {
                assert_eq!(backend::bundled_input_ticket(&settled)?, &result["ticket"]);
            }
            let saved = fs::read(&path)?;
            let duplicate = Resume::parse(&args)?.run_in(state.clone(), |_, _, _, _, _| {
                panic!("a recorded resume must not start another replacement")
            })?;
            assert_eq!(duplicate["result"], result["result"]);
            assert_eq!(duplicate["audit"], result["audit"]);
            assert_eq!(duplicate["attempt"], result["attempt"]);
            assert!(fs::read(&path)? == saved);
        } else {
            assert_eq!(starts, 0);
            match case {
                "completed" | "wrong-owner" | "legacy-fence" => assert!(result.is_err()),
                _ => assert_eq!(result?["error"], "lifecycle_reconcile_required"),
            }
            assert!(fs::read(&path)? == before);
        }
        assert!(fs::read(&close_path).ok() == close_before);
        assert!(fs::read(&cleanup).ok() == signed_before);
        assert_eq!(fs::read_to_string(&report)?, "ordinary preserved report\n");
        assert!(fs::read(&ledger).ok() == ledger_before);
    }
    Ok(())
}
