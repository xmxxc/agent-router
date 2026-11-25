use super::*;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bundled_zero_write_recovery_never_sends_input() -> Result<()> {
    for (operation, outcome, may_have_written) in [
        ("nudge", "definitely-not-sent", false),
        ("continue", "definitely-not-sent", false),
        ("nudge", "uncertain", true),
        ("nudge", "definitely-not-sent", true),
    ] {
        let fixture =
            Fixture(std::env::temp_dir().join(format!("wfc-bundled-zero-{}", Uuid::new_v4())));
        let state = &fixture.0;
        let dispatch = "d-1234abcd";
        let attempt = "a-0123456789ab";
        let dir = state.join("dispatches");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{dispatch}.json"));
        let report = state.join("first-report.md");
        let task = state.join("next-task.md");
        let next_report = state.join("next-report.md");
        fs::write(&task, "ordinary next task\n")?;
        let ticket =
            json!({"version":2,"unit":dispatch,"attempt":attempt,"nonce":"fixture-ticket"});
        let mut original = json!({"dispatch":dispatch,"round":1,"attempt":attempt,"role":"worker",
            "scope":"input-fixture","host":"codex","backend":"bundled","report":report,
            "backendHandle":ticket,"create":{"result":"accepted","round":1,"attempt":attempt,
                "dispatch":dispatch,"ticket":ticket},"audit":[],"workerCallbackHash":"a".repeat(64)});
        let mut args = vec![OsString::from(dispatch)];
        if operation == "continue" {
            original["messages"] = json!([{"dispatch":dispatch,"round":1,"attempt":attempt,
                "type":"done","id":17,"messageId":"m-original-done","state":"processed",
                "statusVersion":3,"report":report}]);
            original["rounds"] = json!([{"round":1,"attempt":attempt,"task":task,"report":report,
                "state":"completed","result":{"id":17,"messageId":"m-original-done"}}]);
            args.extend([
                OsString::from("--task"),
                task.into_os_string(),
                OsString::from("--report"),
                next_report.into_os_string(),
            ]);
        } else {
            args.extend(
                ["--text", "ordinary reminder", "--message-id", "original"].map(OsString::from),
            );
        }
        save(state, &path, &original)?;

        let fence = state.join("router.db-journal");
        let marker = b"ordinary synthetic legacy write fence";
        let failed = Request::parse(operation, &args)?
            .run_in(
                state.clone(),
                |_, _| {
                    Ok((
                        json!({"state":"running"}),
                        json!({"result":"accepted","alive":true,"owned":true,"ticket":ticket}),
                    ))
                },
                |send, prepare, check_write| {
                    assert_eq!(send.operation, operation);
                    assert_eq!(send.row["backendHandle"], ticket);
                    assert!(send.text.starts_with("[router] "));
                    prepare().expect("authorized synthetic preparation");
                    fs::write(&fence, marker).expect("create owned test marker");
                    let refusal = check_write()
                        .expect_err("the regular write fence must stop the next write");
                    assert!(refusal.to_string().contains("legacy state write blocked"));
                    (Ok(json!({"result":outcome})), may_have_written)
                },
                |_, _| panic!("a new input never looks up a recovery ticket"),
            )
            .expect_err("normal settlement must remain fenced");
        let staged = super::super::read_record(&path)?;
        let pending_bytes = fs::read(&path)?;
        assert_eq!(staged["audit"][0]["result"], "pending");
        assert_eq!(fs::read(&fence)?, marker);
        assert!(!state.join("messages.json").exists());

        let retry = |args: &[OsString], ticket_matches| {
            Request::parse(operation, args)?.run_in(
                state.clone(),
                |_, _| panic!("recovery must never inspect a live transport"),
                |_, _, _| panic!("recovery must never enter the transport"),
                |recovery_state, selected| {
                    assert_eq!(recovery_state, state);
                    assert_eq!(selected, &ticket);
                    ensure!(
                        ticket_matches,
                        "fixture no longer identifies the original execution"
                    );
                    Ok(())
                },
            )
        };
        if may_have_written {
            assert!(!failed.is::<controller::ControllerError>());
            fs::remove_file(&fence)?;
            assert!(retry(&args, true).is_err());
            assert_eq!(fs::read(&path)?, pending_bytes);
            continue;
        }

        let issued: Value = serde_json::from_str(
            &failed
                .downcast_ref::<controller::ControllerError>()
                .context("missing structured zero-write response")?
                .to_string(),
        )?;
        assert_eq!(issued["error"], "bundled_no_send_recovery_required");
        assert_eq!(issued["result"], "definitely-not-sent");
        assert_eq!(issued["audit"], staged["audit"][0]["id"]);
        let proof = issued["noSendProof"]
            .as_str()
            .context("missing no-send proof")?;
        assert!(!staged.to_string().contains(proof));
        let mut with_proof = args.clone();
        with_proof.extend([OsString::from("--no-send-proof"), OsString::from(proof)]);
        assert!(retry(&with_proof, true)
            .expect_err("recovery remains fenced")
            .to_string()
            .contains("legacy state write blocked"));
        assert_eq!(fs::read(&fence)?, marker);
        fs::remove_file(&fence)?;
        assert!(retry(&args, true).is_err());
        let mut wrong_request = with_proof.clone();
        if operation == "nudge" {
            wrong_request[4] = OsString::from("different-key");
        } else {
            wrong_request[4] = state.join("different-report.md").into_os_string();
        }
        assert!(retry(&wrong_request, true).is_err());
        assert!(retry(&with_proof, false).is_err());
        assert_eq!(fs::read(&path)?, pending_bytes);

        {
            let mut inbox = messages::Inbox::open(state)?;
            let mut body = staged["audit"][0].clone();
            body["inputEvidence"] = json!(backend::BUNDLED_APPENDED_USER_ENTRY);
            inbox.publish_worker_input(&staged, &body)?;
        }
        let ledger = state.join("messages.json");
        let conflicting_receipt = fs::read(&ledger)?;
        assert!(retry(&with_proof, true)
            .expect_err("a synthetic receipt conflicts with zero-write recovery")
            .to_string()
            .contains("worker history conflicts"));
        assert_eq!(fs::read(&path)?, pending_bytes);
        assert_eq!(fs::read(&ledger)?, conflicting_receipt);
        fs::remove_file(&ledger)?;

        let settled = retry(&with_proof, true)?;
        assert_eq!(settled["audit"], issued["audit"]);
        assert_eq!(settled["result"], "definitely-not-sent");
        assert_eq!(settled["attempt"], attempt);
        let mut final_row = super::super::read_record(&path)?;
        assert!(final_row.get(NO_SEND_RECOVERY).is_none());
        let audit = final_row["audit"][0].clone();
        assert_eq!(audit["id"], issued["audit"]);
        assert_eq!(audit["result"], "definitely-not-sent");
        assert_eq!(audit["inputEvidence"], "bundled-first-write-blocked");
        final_row["audit"] = json!([]);
        assert_eq!(final_row, original);
        assert!(!state.join("messages.json").exists());
        assert!(retry(&with_proof, true).is_err());
    }
    Ok(())
}

#[test]
fn bundled_continue_finishes_matching_first_publication() -> Result<()> {
    for case in [
        "matching",
        "late-attempt",
        "late-task",
        "missing-startup-mode",
        "unverified-published",
        "legacy-fence",
        "wrong-owner",
    ] {
        let fixture =
            Fixture(std::env::temp_dir().join(format!("wfc-publication-{}", Uuid::new_v4())));
        let state = &fixture.0;
        let dir = state.join("dispatches");
        fs::create_dir_all(&dir)?;
        let state = &state.canonicalize()?;
        let dir = state.join("dispatches");
        let dispatch = "d-1234abcd";
        let physical = "a-0123456789ab";
        let scope = "input-fixture";
        let owner = "synthetic-owner-token";
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        write_json(
            &state.join("controller-owners.json"),
            &json!({(scope):{"owner":"fixture","generation":7,"until":now + 600.0,
                "fingerprint":messages::body_hash(owner),"retired":[]}}),
            None,
        )?;
        let task = state.join("first-task.md");
        let report = state.join("first-report.md");
        let next_task = state.join("next-task.md");
        let next_report = state.join("next-report.md");
        fs::write(&next_task, "ordinary next task\n")?;
        let reserve = json!({"dispatch":dispatch,"round":1,"attempt":physical,"role":"worker",
            "scope":scope,"host":"codex","backend":"bundled","cwd":state,"task":task,"report":report,
            "created":now,"worker":{"state":"running"},"bundledStartupCommit":true,
            "workerCallbackHash":"a".repeat(64)});
        let pending = assignment::reserve(&dir, dispatch, &reserve)?;
        let ticket =
            json!({"version":2,"unit":dispatch,"attempt":physical,"nonce":"fixture-ticket"});
        let archive = json!([{"dispatch":dispatch,"round":1,"attempt":physical,"type":"done",
            "id":17,"messageId":"m-original-done","state":"processed","statusVersion":3,"report":report}]);
        let mut published = reserve.clone();
        published["backendHandle"] = ticket.clone();
        published["create"] = json!({"result":"accepted","dispatch":dispatch,"round":1,
            "attempt":physical,"ticket":ticket});
        published["messages"] = archive.clone();
        if case == "unverified-published" {
            published["publicationDiagnostic"] = json!("synthetic unverified publication");
        }
        let path = dir.join(format!("{dispatch}.json"));
        save(state, &path, &published)?;
        let published_bytes = fs::read(&path)?;
        let mut late_reserve = reserve.clone();
        match case {
            "late-attempt" => late_reserve["attempt"] = json!("a-0123456789cd"),
            "late-task" => late_reserve["task"] = json!(state.join("different-task.md")),
            "missing-startup-mode" => {
                late_reserve
                    .as_object_mut()
                    .context("fixture reservation must be an object")?
                    .remove("bundledStartupCommit");
            }
            _ => (),
        }
        let args = [
            OsString::from(dispatch),
            OsString::from("--task"),
            next_task.into_os_string(),
            OsString::from("--report"),
            next_report.into_os_string(),
            OsString::from("--owner-token"),
            OsString::from(if case == "wrong-owner" {
                "wrong-token"
            } else {
                owner
            }),
        ];
        let answer = Request::parse("continue", &args)?.run_in(
            state.clone(),
            |_, _| {
                if late_reserve != reserve {
                    write_json(&pending, &late_reserve, None)?;
                }
                if case == "legacy-fence" {
                    fs::write(state.join("router.db-journal"), b"synthetic legacy fence")?;
                }
                Ok((
                    json!({"state":"running"}),
                    json!({"result":"accepted","alive":true,"owned":true,"ticket":ticket}),
                ))
            },
            |send, prepare, check_write| {
                assert_eq!(case, "matching");
                assert_eq!(send.row["backendHandle"], ticket);
                assert!(!pending.try_exists().expect("read reservation entry"));
                let ready = prepare().and_then(|()| check_write());
                (
                    ready.map(|()| json!({"result":"accepted","inputEvidence":backend::BUNDLED_APPENDED_USER_ENTRY})),
                    true,
                )
            },
            |_, _| panic!("ordinary continuation does not verify a zero-send proof"),
        );
        if case == "matching" {
            let response = answer?;
            assert_eq!(response["round"], 2);
            assert_ne!(response["attempt"], physical);
            let after = messages::read_callback_record(&dir, dispatch)?;
            assert_eq!(after["attempt"], response["attempt"]);
            assert_eq!(after["executionId"], physical);
            assert_eq!(backend::bundled_input_ticket(&after)?, &ticket);
            assert_eq!(after["create"], published["create"]);
            assert_eq!(after["messages"], archive);
            assert_eq!(after["rounds"][0]["result"]["messageId"], "m-original-done");
            assert_eq!(after["audit"][0]["ownerGeneration"], 7);
            assert!(!pending.try_exists()?);
            let receipts = messages::history(state, dispatch)?;
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0]["id"], response["message"]);
        } else {
            let error = answer.expect_err("an unverified publication must not advance");
            let diagnostic = error.to_string();
            let expected = match case {
                "legacy-fence" => "legacy state write blocked",
                "wrong-owner" => "stale_controller_owner",
                _ => assignment::UNVERIFIED_ASSIGNMENT,
            };
            assert!(diagnostic.contains(expected), "{diagnostic}");
            assert!(fs::read(&path)? == published_bytes);
            assert!(super::super::read_record(&pending)? == late_reserve);
            assert!(!state.join("messages.json").try_exists()?);
        }
    }
    Ok(())
}
