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
fn bundled_zero_write_recovery_preserves_existing_close_binding() -> Result<()> {
    for cleaned in [false, true] {
        let fixture =
            Fixture(std::env::temp_dir().join(format!("wfc-close-binding-{}", Uuid::new_v4())));
        fs::create_dir(&fixture.0)?;
        let state = &fixture.0;
        let dir = state.join("dispatches");
        fs::create_dir(&dir)?;
        let dispatch = "d-1234abcd";
        let physical = "a-0123456789ab";
        let logical = "a-0123456789cd";
        let scope = "input-fixture";
        let (ticket, cleanup) =
            crate::bundled::ticket_cleanup_fixture(state, dispatch, physical, cleaned)?;
        let path = dir.join(format!("{dispatch}.json"));
        let report = state.join("first-report.md");
        let task = state.join("next-task.md");
        let next_report = state.join("next-report.md");
        fs::write(&task, "ordinary next task\n")?;

        let original = json!({"dispatch":dispatch,"round":1,"attempt":physical,"role":"worker",
            "scope":scope,"host":"codex","backend":"bundled","report":report,"backendHandle":ticket,
            "create":{"result":"accepted","round":1,"attempt":physical,"dispatch":dispatch,"ticket":ticket},
            "audit":[],"rounds":[{"round":1,"attempt":physical,"state":"completed","report":report}]});
        let mut pending = original.clone();
        pending["round"] = json!(2);
        pending["attempt"] = json!(logical);
        pending["executionId"] = json!(physical);
        pending["task"] = json!(task);
        pending["report"] = json!(next_report);
        pending["audit"] = json!([{"id":"i-close-fixture","operation":"continue","result":"pending",
            "round":2,"attempt":logical,"fromRound":1,"fromAttempt":physical,
            "task":task,"report":next_report}]);
        let proof = Uuid::new_v4().simple().to_string().repeat(2);
        recovery::stage(&original, &mut pending, &proof, 0)?;
        save(state, &path, &pending)?;
        let old_message = json!({"id":17,"messageId":"m-original-done","type":"done",
            "dispatch":dispatch,"round":1,"attempt":physical,"role":"worker","scope":scope,
            "report":report,"state":"processed","statusVersion":3});
        messages::stage_legacy_messages(state, &[old_message])?;

        // The closing record names the physical ticket and the current
        // business round, as it does when a close follows a staged continue.
        let intent = dir.join(format!("{dispatch}.{physical}.closing"));
        write_json(
            &intent,
            &json!({"ticket":ticket,"round":2,"workerOnClose":null}),
            None,
        )?;
        let ledger = state.join("messages.json");
        let preserved = [&path, &intent, &ledger];
        let before = [fs::read(&path)?, fs::read(&intent)?, fs::read(&ledger)?];
        let cleanup_before = if cleaned {
            Some(fs::read(&cleanup)?)
        } else {
            None
        };
        let status_before = backend::observe(state, &mut pending.clone())?;
        let receipt_before = backend::read_only_close(state, &pending)?;
        assert_eq!(status_before.1["ticket"], ticket);
        assert_eq!(receipt_before.is_some(), cleaned);
        if let Some(receipt) = &receipt_before {
            assert_eq!(receipt["cleanup"], "confirmed");
            assert_eq!(receipt["ticket"], ticket);
            assert_eq!(receipt["round"], 2);
            assert_eq!(receipt["attempt"], logical);
        }

        let args = [
            OsString::from(dispatch),
            OsString::from("--task"),
            task.into_os_string(),
            OsString::from("--report"),
            next_report.into_os_string(),
            OsString::from("--no-send-proof"),
            OsString::from(proof),
        ];
        let refusal = Request::parse("continue", &args)?
            .run_in(
                state.clone(),
                |_, _| panic!("recovery must never inspect a live transport"),
                |_, _, _| panic!("recovery must never enter the transport"),
                verify_bundled_ticket,
            )
            .expect_err("the original close intent must keep the continuation pending");
        assert!(refusal.to_string().contains("close has an existing intent"));
        for (file, bytes) in preserved.into_iter().zip(before) {
            assert!(fs::read(file)? == bytes, "fixture file changed: {file:?}");
        }
        if let Some(bytes) = cleanup_before {
            assert!(
                fs::read(&cleanup)? == bytes,
                "signed cleanup receipt changed"
            );
        } else {
            assert!(
                !cleanup.try_exists()?,
                "recovery published a cleanup receipt"
            );
        }
        let mut after = super::super::read_record(&path)?;
        assert_eq!(backend::observe(state, &mut after)?, status_before);
        assert_eq!(backend::read_only_close(state, &after)?, receipt_before);
    }
    Ok(())
}
