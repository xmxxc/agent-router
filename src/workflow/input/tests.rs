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
