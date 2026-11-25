//! An unfinished first-round Codex task can move to a fresh bundled OpenCode worker.
use super::*;
use crate::workflow::{bind_worker_callback, write_json};

pub(super) fn run(request: Handoff<'_>, state: &Path) -> Result<Value> {
    ensure!(
        request.host == "opencode" && request.next.is_none(),
        "bundled handoff supports the unfinished Codex task to OpenCode without --task or --report"
    );
    let dir = state.join("dispatches");
    let path = dir.join(format!("{}.json", request.dispatch));
    let read = || messages::read_callback_record(&dir, request.dispatch);
    let initial = read()?;
    worker_owner::validate_close(&initial)?;
    let scope = super::super::record_scope(&initial).context("invalid dispatch scope")?;
    let token = controller::token(request.owner_token);
    let owner_generation = || -> Result<Value> {
        let owners = controller::Owners::read(state)?;
        owners.require(scope, token.as_deref(), "handoff")?;
        Ok(owners.public(scope)?.unwrap_or(Value::Null)["generation"].clone())
    };
    owner_generation()?;
    let _scope = controller::lock_scope(state, scope)?;
    let generation = owner_generation()?;
    let row = read()?;
    ensure!(row == initial, "assignment changed while preparing handoff");
    if let Some(event) = assignment::unresolved_lifecycle(&row) {
        let Some((index, pending)) = bundled_event(&row).filter(|(_, pending)| *pending == event)
        else {
            return Ok(unresolved(
                &row,
                event,
                "previous lifecycle operation is unresolved",
            ));
        };
        ensure!(
            pending["requestedModel"] == json!(request.model),
            "handoff retry does not match the original requested model"
        );
        ensure!(
            pending["ownerGeneration"] == generation
                && read_scope_binding(state, scope)? == Some(scope_binding(&row, pending)),
            "bundled handoff retry does not match the original scope binding"
        );
        if !starting_target(&row, index) {
            let mut reply = unresolved(
                &row,
                pending,
                "original handoff requires close reconciliation",
            );
            if let Ok(Some(closed)) = backend::read_only_close(state, &row) {
                if let Ok(ticket) = backend::bundled_input_ticket(&row) {
                    if verified_stop(&row, ticket, &closed) {
                        reply["close"] = closed;
                    }
                }
            }
            return Ok(reply);
        }
        return recover_launch(state, scope, &row, index, || {
            ensure!(
                owner_generation()? == generation,
                "handoff owner changed before receipt recovery"
            );
            Ok(())
        });
    }
    if let Some((index, event)) = bundled_event(&row)
        .filter(|(_, event)| row["host"] == "opencode" && row["attempt"] == event["toAttempt"])
    {
        ensure!(
            event["requestedModel"] == json!(request.model)
                && event["result"] == "accepted"
                && event["phase"] == "complete"
                && launch_matches(&event["launch"], &row["dispatch"], &row["attempt"])
                && row["create"] == event["launch"]
                && has_bound_stop(&row, event),
            "recorded bundled handoff does not match this request"
        );
        let mut reply = launch_response(&row, index);
        reply["duplicate"] = json!(true);
        return Ok(reply);
    }

    ensure!(
        row["backend"] == "bundled"
            && row["host"] == "codex"
            && row["round"] == 1
            && assignment::identified(&row)
            && row["create"]["identity"].is_object(),
        "bundled handoff requires an accepted first-round Codex assignment"
    );
    let ordering = ordering_event(&row, request.model)?;
    let same_order = if let Some((_, event)) = ordering {
        read_scope_binding(state, scope)? == Some(scope_binding(&row, event))
    } else {
        false
    };
    if same_order {
        require_interrupt_settled(state, scope)?;
    } else {
        require_scope_settled(state, scope)?;
    }
    let ticket = backend::bundled_input_ticket(&row)?.clone();
    let execution = assignment::close_lock(state, &row)?;
    let refuse = |reason: &str| -> Result<Value> {
        let Some((index, _)) = ordering else {
            anyhow::bail!("{reason}");
        };
        let _inbox = messages::lock_for_publish(state)?;
        ensure!(
            owner_generation()? == generation && read()? == row,
            "assignment or owner changed before settling the unstarted handoff"
        );
        let mut settled = row.clone();
        withdraw_order(&mut settled, index, reason);
        write_json(&path, &settled, None)?;
        messages::sync_directory(&dir)?;
        let mut reply = launch_response(&settled, index);
        reply["diagnostic"] = json!(reason);
        Ok(reply)
    };
    if ordering.is_some_and(|(_, event)| event["ownerGeneration"] != generation) {
        return refuse("handoff owner changed before closing began");
    }
    let (worker, seen) = match backend::observe_with_mode(state, &mut row.clone(), true) {
        Ok(observed) => observed,
        Err(error) => {
            return refuse(&format!(
                "cannot verify the original bundled execution: {error}"
            ))
        }
    };
    if seen["result"] != "accepted"
        || seen["ticket"] != ticket
        || seen["alive"] != true
        || seen["owned"] != true
        || worker["state"] != "running"
    {
        return refuse("original bundled writer is not verified live");
    }
    crate::bundled::require_delegation()?;
    let mut next = request.prepare_record(&row, scope, request.model)?;
    if let Some((_, event)) = ordering {
        next["attempt"] = event["toAttempt"].clone();
        next["executionId"] = event["toAttempt"].clone();
    }
    let options = host::Options {
        name: request.host,
        backend: "bundled",
        config: row["launchConfig"].as_str(),
        model: request.model,
        git: row["git"] == true,
        network: row["network"] == true,
        ..host::Options::default()
    };
    next.as_object_mut()
        .context("invalid handoff assignment")?
        .remove("effort");
    let mut launch = options.prepare_handoff(state, &mut next)?;
    bind_worker_callback(&mut next, Some(&mut launch));
    if std::env::var_os("WFC_BUNDLED_CGROUP_ROOT").is_none() {
        next["bundledStartupCommit"] = json!(true);
    } else if let Some(record) = next.as_object_mut() {
        record.remove("bundledStartupCommit");
    }

    let mut closing = row.clone();
    let index = {
        let inbox = messages::Inbox::open(state)?;
        ensure!(
            owner_generation()? == generation && read()? == row,
            "assignment or owner changed while preparing bundled handoff"
        );
        ensure!(
            !assignment::input_unresolved(&row) && !inbox.unresolved_reply(&row, None)?,
            "original bundled input is unresolved"
        );
        let history = view::merged_history(&row, messages::history(state, request.dispatch)?)?;
        let mut projected = row.clone();
        projected["worker"] = worker.clone();
        view::project(&mut projected, &history, Some(true), false)?;
        if projected["state"] != "working" {
            drop(inbox);
            return refuse(
                "bundled handoff requires an unfinished working task; a completed task needs a new round",
            );
        }
        inbox.finish_hosted_publication(request.dispatch, &row, token.as_deref(), "handoff")?;

        let mut rounds = projected["rounds"]
            .as_array()
            .cloned()
            .context("missing round history")?;
        let last = rounds.last_mut().context("missing original round")?;
        ensure!(
            last["round"] == row["round"] && last["attempt"] == row["attempt"],
            "original handoff round changed"
        );
        last["state"] = json!("unknown");
        closing["rounds"] = json!(rounds);
        let now = chrono::Utc::now();
        rounds.push(
            json!({"round":next["round"],"attempt":next["attempt"],"task":next["task"],
            "report":next["report"],"created":now.timestamp_micros() as f64 / 1_000_000.0,
            "completed":null,"state":"unknown","result":null}),
        );
        next["rounds"] = json!(rounds);
        closing[PENDING_CALLBACK_HASH] = next[controller::CALLBACK_HASH].clone();
        if closing["audit"].is_null() {
            closing["audit"] = json!([]);
        }
        let audit = closing["audit"]
            .as_array_mut()
            .context("invalid lifecycle audit")?;
        let index = ordering.map_or(audit.len(), |(index, _)| index);
        if ordering.is_none() {
            audit.push(json!({
                "id":index + 1,"operation":"handoff","dispatch":request.dispatch,
                "round":next["round"],"attempt":next["attempt"],
                "fromRound":row["round"],"fromAttempt":row["attempt"],"toAttempt":next["attempt"],
                "fromHost":row["host"],"toHost":next["host"],"fromModel":row["model"],"toModel":next["model"],
                "fromTask":row["task"],"fromReport":row["report"],"toTask":next["task"],"toReport":next["report"],
                "ownerGeneration":generation,"at":now.to_rfc3339(),"result":"pending","phase":"ordering",
                "fromBackend":"bundled","target":{"backend":"bundled"},"requestedModel":request.model,
                "priorCreate":row["create"]
            }));
        }
        // A crash before the order installs leaves the same numbered audit
        // visible without changing the old worker's running round or callback.
        let ordered = ordering_snapshot(&row, &closing);
        write_json(&path, &ordered, None)?;
        messages::sync_directory(&dir)?;
        write_json(
            &scope_path(state, scope),
            &scope_binding(&ordered, &ordered["audit"][index]),
            None,
        )?;
        messages::sync_directory(state)?;
        closing["audit"][index]["phase"] = json!("closing");
        write_json(&path, &closing, None)?;
        messages::sync_directory(&dir)?;
        index
    };
    drop(execution);

    let event = &closing["audit"][index];
    let advance = || -> Result<Value> {
        ensure!(
            owner_generation()? == generation,
            "handoff owner changed before close"
        );
        let closed = worker_owner::close_for_handoff_locked(state, &closing, index + 1)?;
        ensure!(
            verified_stop(&row, &ticket, &closed),
            "original bundled close did not confirm the same execution: {}",
            closed["diagnostic"]
                .as_str()
                .unwrap_or("missing exact cleanup receipt")
        );
        let _original_execution = assignment::close_lock(state, &row)?;
        ensure!(
            owner_generation()? == generation && read()? == closing,
            "assignment or owner changed after original close"
        );
        crate::bundled::apply_handoff_cleanup(state, &ticket, &closed["proof"])?;
        let _target_execution = assignment::close_lock(state, &next)?;
        {
            let _inbox = messages::lock_for_publish(state)?;
            ensure!(
                owner_generation()? == generation && read()? == closing,
                "assignment or owner changed before bundled handoff launch"
            );
            next["audit"] = closing["audit"].clone();
            next["audit"][index]["close"] = closed.clone();
            next["audit"][index]["oldWriterStopped"] = json!(true);
            next["audit"][index]["writerStopEvidence"] = closed["proof"].clone();
            next["audit"][index]["phase"] = json!("starting");
            write_json(&path, &next, None)?;
            messages::sync_directory(&dir)?;
        }
        ensure!(
            owner_generation()? == generation
                && read()? == next
                && read_scope_binding(state, scope)?
                    == Some(scope_binding(&next, &next["audit"][index])),
            "handoff assignment, scope or owner changed before launch"
        );
        let authorize_ready = || {
            let Some(inbox) = messages::try_lock_for_publish(state)? else {
                return Ok(None);
            };
            ensure!(
                owner_generation()? == generation
                    && read()? == next
                    && read_scope_binding(state, scope)?
                        == Some(scope_binding(&next, &next["audit"][index])),
                "bundled handoff changed before committing ready"
            );
            Ok(Some((inbox, || -> Result<()> {
                ensure!(
                    owner_generation()? == generation,
                    "handoff owner changed before ready"
                );
                Ok(())
            })))
        };
        let reply = backend::create_bundled_handoff(state, &next, launch, &ticket, authorize_ready)
            .unwrap_or_else(|error| json!({"result":"uncertain","diagnostic":error.to_string()}));
        let _inbox = messages::lock_for_publish(state)?;
        ensure!(
            owner_generation()? == generation && read()? == next,
            "assignment or owner changed before bundled handoff settlement"
        );
        settle_launch(&mut next, index, reply);
        write_json(&path, &next, None)?;
        messages::sync_directory(&dir)?;
        Ok(launch_response(&next, index))
    };
    Ok(advance().unwrap_or_else(|error| unresolved(&closing, event, &error.to_string())))
}

fn starting_target(row: &Value, index: usize) -> bool {
    let pending = &row["audit"][index];
    pending["id"] == index + 1
        && pending["phase"] == "starting"
        && pending["attempt"] == row["attempt"]
        && pending["toAttempt"] == row["attempt"]
        && pending["fromAttempt"] != row["attempt"]
        && pending["round"] == row["round"]
        && pending["fromRound"] == row["round"]
        && row["round"] == 1
        && row["host"] == "opencode"
        && row["backend"] == "bundled"
        && row["executionId"] == row["attempt"]
        && row["createPending"] != true
        && row["rounds"]
            .as_array()
            .and_then(|rounds| rounds.last())
            .is_some_and(|round| {
                round["round"] == row["round"] && round["attempt"] == row["attempt"]
            })
        && has_bound_stop(row, pending)
}

pub(super) fn recover_expired(
    state: &Path,
    scope: &str,
    generation: u64,
    row: &Value,
    index: usize,
) -> Result<()> {
    let pending = &row["audit"][index];
    if pending["ownerGeneration"] != generation
        || bundled_event(row) != Some((index, pending))
        || assignment::unresolved_lifecycle(row) != Some(pending)
        || !starting_target(row, index)
    {
        return Ok(());
    }
    worker_owner::validate_close(row)?;
    recover_launch(state, scope, row, index, || {
        let owner = controller::Owners::read(state)?
            .public(scope)?
            .unwrap_or(Value::Null);
        ensure!(
            owner["generation"] == generation && owner["active"] == false,
            "expired handoff owner changed before receipt recovery"
        );
        Ok(())
    })?;
    Ok(())
}

fn recover_launch(
    state: &Path,
    scope: &str,
    row: &Value,
    index: usize,
    authorize: impl FnOnce() -> Result<()>,
) -> Result<Value> {
    let dispatch = row["dispatch"].as_str().context("missing dispatch")?;
    let pending = &row["audit"][index];
    // The execution lock precedes the Inbox lock. Recovery only reads the
    // selected generation's signed launch and the original signed close.
    let _execution = assignment::close_lock(state, row)?;
    let Ok(receipt) = crate::bundled::handoff_launch_receipt(
        state,
        dispatch,
        &row["attempt"],
        &pending["close"]["ticket"],
        &pending["close"]["proof"],
    ) else {
        return Ok(unresolved(
            row,
            pending,
            "bundled handoff launch remains uncertain",
        ));
    };
    if !launch_matches(&receipt, &row["dispatch"], &row["attempt"]) {
        return Ok(unresolved(
            row,
            pending,
            "bundled handoff launch does not match the selected target",
        ));
    }
    let ticket_changed = [
        &pending["launch"]["ticket"],
        &row["create"]["ticket"],
        &row["backendHandle"],
    ]
    .into_iter()
    .any(|ticket| !ticket.is_null() && *ticket != receipt["ticket"]);
    let identity_changed = [&pending["launch"]["identity"], &row["create"]["identity"]]
        .into_iter()
        .any(|identity| !identity.is_null() && *identity != receipt["identity"]);
    if ticket_changed || identity_changed {
        return Ok(unresolved(
            row,
            pending,
            "bundled handoff launch ticket or identity changed",
        ));
    }
    let dir = state.join("dispatches");
    let _inbox = messages::lock_for_publish(state)?;
    authorize()?;
    ensure!(
        messages::read_callback_record(&dir, dispatch)? == *row
            && read_scope_binding(state, scope)? == Some(scope_binding(row, pending)),
        "assignment or scope changed before handoff receipt recovery"
    );
    let mut settled = row.clone();
    settle_launch(&mut settled, index, receipt);
    write_json(&dir.join(format!("{dispatch}.json")), &settled, None)?;
    messages::sync_directory(&dir)?;
    let mut reply = launch_response(&settled, index);
    reply["duplicate"] = json!(true);
    Ok(reply)
}

fn bundled_event(row: &Value) -> Option<(usize, &Value)> {
    row["audit"]
        .as_array()?
        .iter()
        .enumerate()
        .rfind(|(_, event)| {
            event["operation"] == "handoff"
                && event["dispatch"] == row["dispatch"]
                && event["fromBackend"] == "bundled"
                && event["fromHost"] == "codex"
                && event["toHost"] == "opencode"
                && event["target"]["backend"] == "bundled"
        })
}

fn ordering_event<'r>(row: &'r Value, model: Option<&str>) -> Result<Option<(usize, &'r Value)>> {
    let Some((index, event)) = bundled_event(row).filter(|(_, event)| event["phase"] == "ordering")
    else {
        return Ok(None);
    };
    ensure!(
        assignment::handoff_preflight(event)
            && event["result"] == "pending"
            && event["id"] == index + 1
            && event["fromAttempt"] == row["attempt"]
            && event["fromRound"] == row["round"]
            && event["fromTask"] == row["task"]
            && event["fromReport"] == row["report"]
            && event["priorCreate"] == row["create"]
            && event["requestedModel"] == json!(model)
            && event["toAttempt"] != row["attempt"]
            && event["toAttempt"]
                .as_str()
                .is_some_and(|id| super::super::valid_identity(id, "a-")),
        "bundled handoff ordering record does not match the original request"
    );
    Ok(Some((index, event)))
}

pub(super) fn order_had_no_backend_effect(event: &Value) -> bool {
    event["fromBackend"] == "bundled"
        && event["target"]["backend"] == "bundled"
        && event["close"].is_null()
        && event["launch"].is_null()
        && matches!(
            (event["phase"].as_str(), event["result"].as_str()),
            (Some("ordering"), Some("pending")) | (Some("withdrawn"), Some("definitely-not-sent"))
        )
}

fn ordering_snapshot(row: &Value, closing: &Value) -> Value {
    let mut ordered = row.clone();
    ordered["audit"] = closing["audit"].clone();
    ordered
}

fn withdraw_order(row: &mut Value, index: usize, diagnostic: &str) {
    let event = &mut row["audit"][index];
    event["phase"] = json!("withdrawn");
    event["result"] = json!("definitely-not-sent");
    event["diagnostic"] = json!(diagnostic);
    row.as_object_mut().unwrap().remove(PENDING_CALLBACK_HASH);
}

fn verified_stop(row: &Value, ticket: &Value, closed: &Value) -> bool {
    closed["operation"] == "close"
        && closed["result"] == "completed"
        && closed["cleanup"] == "confirmed"
        && closed["scope"].is_null()
        && ["dispatch", "round", "attempt"]
            .iter()
            .all(|&key| closed[key] == row[key])
        && closed["ticket"] == *ticket
        && ticket["version"] == 2
        && ticket["unit"] == row["dispatch"]
        && ticket["attempt"] == row["attempt"]
        && closed["proof"]["proves"] == "execution-empty"
        && matches!(
            closed["proof"]["state"].as_str(),
            Some("closed" | "exited" | "faulted")
        )
        && row["create"]["identity"].is_object()
        && closed["proof"]["identity"] == row["create"]["identity"]
}

pub(super) fn has_bound_stop(row: &Value, event: &Value) -> bool {
    let prior = json!({"dispatch":row["dispatch"],"attempt":event["fromAttempt"],
        "round":event["fromRound"],"create":event["priorCreate"]});
    event["oldWriterStopped"] == true
        && event["fromBackend"] == "bundled"
        && event["target"]["backend"] == "bundled"
        && event["dispatch"] == row["dispatch"]
        && event["writerStopEvidence"] == event["close"]["proof"]
        && verified_stop(&prior, &event["priorCreate"]["ticket"], &event["close"])
}

pub(super) fn launch_matches(launch: &Value, dispatch: &Value, attempt: &Value) -> bool {
    launch["result"] == "accepted"
        && launch["ticket"]["version"] == 2
        && launch["ticket"]["unit"] == *dispatch
        && launch["ticket"]["attempt"] == *attempt
        && launch["identity"].is_object()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original() -> (Value, Value, Value) {
        let dispatch = "d-1234abcd";
        let attempt = "a-0123456789ab";
        let ticket = json!({"version":2,"unit":dispatch,"attempt":attempt,"generation":"old"});
        let identity = json!({"worker":{"pid":123,"start_ticks":456}});
        let row = json!({"dispatch":dispatch,"attempt":attempt,"round":1,
            "create":{"result":"accepted","ticket":ticket,"identity":identity}});
        let close = json!({"operation":"close","result":"completed","cleanup":"confirmed",
            "dispatch":dispatch,"round":1,"attempt":attempt,"ticket":ticket,
            "proof":{"state":"closed","proves":"execution-empty","identity":identity}});
        (row, ticket, close)
    }

    #[test]
    fn bundled_handoff_requires_the_original_whole_execution_receipt() {
        let (row, ticket, close) = original();
        assert!(verified_stop(&row, &ticket, &close));
        for (field, value) in [
            ("round", json!(2)),
            ("attempt", json!("a-0123456789cd")),
            ("scope", json!("execution")),
            ("cleanup", json!("unknown")),
            ("ticket", json!({"generation":"other"})),
            (
                "proof",
                json!({"state":"closed","proves":"execution-empty","identity":{}}),
            ),
        ] {
            let mut changed = close.clone();
            changed[field] = value;
            assert!(!verified_stop(&row, &ticket, &changed), "{field}");
        }
    }

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bundled_handoff_ordering_keeps_the_number_and_old_writer() -> Result<()> {
        let state =
            Fixture(std::env::temp_dir().join(format!("wfc-handoff-order-{}", Uuid::new_v4())));
        let dir = state.0.join("dispatches");
        std::fs::create_dir_all(&dir)?;
        let dispatch = "d-1234abcd";
        let attempt = "a-0123456789ab";
        let target = "a-0123456789cd";
        let scope = "handoff-fixture";
        let mut closing = json!({"dispatch":dispatch,"attempt":attempt,"round":1,"host":"codex",
            "backend":"bundled","scope":scope,"rounds":[{"state":"unknown"}],
            (PENDING_CALLBACK_HASH):"new-hash","audit":[{"id":1,"operation":"handoff",
                "result":"pending","phase":"ordering","dispatch":dispatch,"fromAttempt":attempt,
                "fromRound":1,"toAttempt":target,"fromBackend":"bundled","fromHost":"codex",
                "toHost":"opencode","target":{"backend":"bundled"},"ownerGeneration":7}]});
        let original = json!({"dispatch":dispatch,"attempt":attempt,"round":1,"host":"codex",
            "backend":"bundled","scope":scope,"rounds":[{"state":"running"}]});
        let row = ordering_snapshot(&original, &closing);
        assert_eq!(row["rounds"], original["rounds"]);
        assert!(row[PENDING_CALLBACK_HASH].is_null());
        let path = dir.join(format!("{dispatch}.json"));
        write_json(&path, &row, None)?;
        let (_, original) = ordering_event(&row, None)?.unwrap();
        assert!(!assignment::handoff_blocks_old_writer(&row));
        assert!(assignment::unresolved_lifecycle(&row).is_none());
        assert!(read_scope_binding(&state.0, scope)?.is_none());
        require_scope_settled(&state.0, scope)?;
        let order = scope_binding(&row, original);
        write_json(&scope_path(&state.0, scope), &order, None)?;
        require_scope_settled(&state.0, scope)?;
        let same = ordering_event(&row, None)?.unwrap().1;
        assert_eq!(
            read_scope_binding(&state.0, scope)?,
            Some(scope_binding(&row, same))
        );
        assert_eq!(same["id"], 1);
        assert_eq!(same["toAttempt"], target);
        assert!(ordering_event(&row, Some("another-model")).is_err());
        closing["audit"][0]["phase"] = json!("closing");
        assert!(assignment::handoff_blocks_old_writer(&closing));
        write_json(&path, &closing, None)?;
        assert!(require_scope_settled(&state.0, scope).is_err());
        let mut finished = row;
        let done = json!({"id":17,"messageId":"m-old-done","type":"done",
            "dispatch":dispatch,"round":1,"attempt":attempt});
        finished["messages"] = json!([done]);
        withdraw_order(&mut finished, 0, "original task completed before closing");
        write_json(&path, &finished, None)?;
        require_scope_settled(&state.0, scope)?;
        assert_eq!(finished["messages"][0], done);
        assert_eq!(finished["audit"][0]["id"], 1);
        assert_eq!(finished["audit"][0]["result"], "definitely-not-sent");
        assert!(finished["audit"][0]["close"].is_null());
        assert!(!state.0.join("bundled").exists());
        Ok(())
    }

    fn pending_handoff(generation: Value) -> Result<(Fixture, Value)> {
        let state = Fixture(std::env::temp_dir().join(format!("wfc-handoff-{}", Uuid::new_v4())));
        let dir = state.0.join("dispatches");
        std::fs::create_dir_all(&dir)?;
        let state_path = state.0.canonicalize()?;
        let dispatch = "d-1234abcd";
        let target = "a-0123456789cd";
        let scope = "handoff-fixture";
        let (mut prior, _, mut close) = original();
        let original_attempt = prior["attempt"].as_str().unwrap();
        let (old_ticket, _) =
            crate::bundled::ticket_cleanup_fixture(&state_path, dispatch, original_attempt, true)?;
        let old_receipt =
            crate::bundled::assignment_operation(&state_path, &old_ticket, "cleanup-receipt").0;
        assert_eq!(old_receipt["result"], "completed");
        prior["create"]["identity"] = old_receipt["proof"]["identity"].clone();
        prior["create"]["ticket"] = old_ticket.clone();
        close["ticket"] = old_ticket.clone();
        close["proof"] = old_receipt["proof"].clone();
        crate::bundled::apply_handoff_cleanup(&state_path, &old_ticket, &close["proof"])?;
        let event = json!({"id":1,"operation":"handoff","result":"pending","phase":"starting",
            "dispatch":dispatch,"round":1,"fromRound":1,"fromAttempt":prior["attempt"],
            "attempt":target,"toAttempt":target,"fromHost":"codex","toHost":"opencode","fromBackend":"bundled",
            "target":{"backend":"bundled"},"requestedModel":null,"ownerGeneration":generation,
            "priorCreate":prior["create"],"close":close,"oldWriterStopped":true,
            "writerStopEvidence":close["proof"]});
        let row = json!({"dispatch":dispatch,"attempt":target,"executionId":target,"round":1,
            "scope":scope,"role":"fixture","backend":"bundled","host":"opencode",
            "audit":[event],"rounds":[{"round":1,"attempt":target,"state":"unknown"}]});
        let path = dir.join(format!("{dispatch}.json"));
        write_json(&path, &row, None)?;
        write_json(
            &scope_path(&state_path, scope),
            &scope_binding(&row, &event),
            None,
        )?;
        Ok((state, row))
    }

    fn retry_request<'a>(dispatch: &'a str, owner_token: Option<&'a str>) -> Handoff<'a> {
        Handoff {
            dispatch,
            host: "opencode",
            backend: "bundled",
            endpoint: None,
            next: None,
            model: None,
            owner_token,
        }
    }

    #[test]
    fn bundled_handoff_retries_only_recover_the_same_signed_target() -> Result<()> {
        let (state, mut row) = pending_handoff(Value::Null)?;
        let state_path = state.0.canonicalize()?;
        let dispatch = row["dispatch"].as_str().unwrap().to_owned();
        let target = row["attempt"].as_str().unwrap().to_owned();
        let scope = row["scope"].as_str().unwrap().to_owned();
        let close = row["audit"][0]["close"].clone();
        let prior_create = row["audit"][0]["priorCreate"].clone();
        let old_ticket = close["ticket"].clone();
        let path = state_path
            .join("dispatches")
            .join(format!("{dispatch}.json"));
        let request = || retry_request(&dispatch, None);
        let initial = std::fs::read(&path)?;
        for _ in 0..2 {
            let retry = run(request(), &state_path)?;
            assert_eq!(retry["result"], "uncertain");
            assert_eq!(retry["audit"], 1);
            assert_eq!(retry["attempt"], target);
            assert_eq!(std::fs::read(&path)?, initial);
            assert_eq!(
                crate::bundled::lookup_assignment(&state_path, &dispatch, &old_ticket["attempt"])?
                    ["ticket"],
                old_ticket
            );
        }
        let (ticket, _) =
            crate::bundled::ticket_cleanup_fixture(&state_path, &dispatch, &target, false)?;
        std::fs::remove_file(
            state_path
                .join("bundled")
                .join(&dispatch)
                .join(old_ticket["generation"].as_str().unwrap())
                .join("superseded.json"),
        )?;
        assert_eq!(run(request(), &state_path)?["result"], "uncertain");
        crate::bundled::bind_handoff_fixture(&state_path, &old_ticket, &ticket)?;
        row["audit"][0]["launch"] =
            json!({"result":"uncertain","ticket":{"attempt":target,"generation":"other"}});
        write_json(&path, &row, None)?;
        let retry = run(request(), &state_path)?;
        assert_eq!(retry["result"], "uncertain");
        assert_eq!(retry["audit"], 1);
        assert_eq!(super::super::super::read_record(&path)?, row);
        row["audit"][0]["launch"]["ticket"] = ticket.clone();
        write_json(&path, &row, None)?;
        let recovered = run(request(), &state_path)?;
        assert_eq!(recovered["result"], "accepted");
        assert_eq!(recovered["duplicate"], true);
        assert_eq!(recovered["audit"], 1);
        assert_eq!(recovered["ticket"], ticket);
        assert_eq!(run(request(), &state_path)?, recovered);
        require_scope_settled(&state_path, &scope)?;
        let settled = super::super::super::read_record(&path)?;
        assert_eq!(settled["audit"][0]["close"], close);
        assert_eq!(settled["audit"][0]["priorCreate"], prior_create);
        Ok(())
    }

    #[test]
    fn bundled_handoff_owner_transition_recovers_only_the_expired_signed_attempt() -> Result<()> {
        for action in ["acquire", "takeover"] {
            let (state, pending) = pending_handoff(json!(7))?;
            let state = &state.0;
            let dispatch = pending["dispatch"].as_str().unwrap();
            let target = pending["attempt"].as_str().unwrap();
            let scope = pending["scope"].as_str().unwrap();
            let original = &pending["audit"][0];
            let old_token = "fixture-old-controller";
            let new_token = "fixture-new-controller";
            let owner_path = state.join("controller-owners.json");
            let dir = state.join("dispatches");
            let row_path = dir.join(format!("{dispatch}.json"));
            let old = || retry_request(dispatch, Some(old_token));
            let args = [
                "--scope",
                scope,
                "--token",
                new_token,
                "--owner",
                "replacement",
            ]
            .map(OsString::from);
            let transition = || controller::Owner::parse(action, &args)?.run_in(state);
            let renew_args = ["--scope", scope, "--token", old_token].map(OsString::from);
            let renew = || controller::Owner::parse("renew", &renew_args)?.run_in(state);
            let read_owners =
                || -> Result<Value> { Ok(serde_json::from_slice(&std::fs::read(&owner_path)?)?) };
            let mut owners = json!({
                (scope): {"owner":"original","generation":7,"until":0.0,
                    "fingerprint":messages::body_hash(old_token),"retired":[]},
                "other-scope": {"owner":"other","generation":11,"until":0.0,
                    "fingerprint":messages::body_hash("other-owner"),"retired":[]}
            });
            write_json(&owner_path, &owners, None)?;
            assert!(run(old(), state)
                .unwrap_err()
                .is::<controller::ControllerError>());
            assert!(renew().unwrap_err().is::<controller::ControllerError>());
            assert!(transition()
                .unwrap_err()
                .to_string()
                .contains("audit 1 is unresolved"));
            assert_eq!(messages::read_callback_record(&dir, dispatch)?, pending);
            assert_eq!(read_owners()?, owners);

            let (ticket, _, identity) = crate::bundled::resume_ticket_fixture(
                state,
                dispatch,
                target,
                crate::bundled::ResumeFixture::Ready,
                true,
            )?;
            let check_fenced = || -> Result<()> {
                assert!(transition().is_err());
                assert_eq!(messages::read_callback_record(&dir, dispatch)?, pending);
                Ok(())
            };
            owners[scope]["until"] = json!(chrono::Utc::now().timestamp() + 600);
            write_json(&owner_path, &owners, None)?;
            check_fenced()?;
            owners[scope]["until"] = json!(0.0);
            owners[scope]["generation"] = json!(8);
            write_json(&owner_path, &owners, None)?;
            check_fenced()?;
            owners[scope]["generation"] = json!(7);
            write_json(&owner_path, &owners, None)?;

            let unit = state.join("bundled").join(dispatch);
            let ready = unit
                .join(ticket["generation"].as_str().unwrap())
                .join("startup-decision.json");
            let cleanup = unit
                .join(original["close"]["ticket"]["generation"].as_str().unwrap())
                .join("cleanup.json");
            for proof in [ready, cleanup] {
                let signed = std::fs::read(&proof)?;
                let mut unsigned: Value = serde_json::from_slice(&signed)?;
                unsigned["signature"] = Value::Null;
                write_json(&proof, &unsigned, None)?;
                check_fenced()?;
                std::fs::write(&proof, signed)?;
            }

            let scope_path = scope_path(state, scope);
            let binding = scope_binding(&pending, original);
            let mut wrong_scope = binding.clone();
            wrong_scope["scope"] = json!("other-scope");
            write_json(&scope_path, &wrong_scope, None)?;
            check_fenced()?;
            write_json(&scope_path, &binding, None)?;

            let mut wrong_attempt = pending.clone();
            wrong_attempt["audit"][0]["attempt"] = original["fromAttempt"].clone();
            let mut wrong_ticket = pending.clone();
            wrong_ticket["audit"][0]["launch"] =
                json!({"result":"uncertain","ticket":{"attempt":target,"generation":"other"}});
            let mut wrong_identity = pending.clone();
            wrong_identity["create"] = json!({"result":"uncertain","ticket":ticket,"identity":{}});
            for changed in [wrong_attempt, wrong_ticket, wrong_identity] {
                write_json(&row_path, &changed, None)?;
                assert!(transition()
                    .unwrap_err()
                    .to_string()
                    .contains("audit 1 is unresolved"));
                assert_eq!(messages::read_callback_record(&dir, dispatch)?, changed);
            }
            write_json(&row_path, &pending, None)?;
            assert_eq!(read_owners()?, owners);

            assert_eq!(transition()?["generation"], 8);
            let settled = messages::read_callback_record(&dir, dispatch)?;
            let audit = &settled["audit"][0];
            assert_eq!(settled["audit"].as_array().unwrap().len(), 1);
            assert_eq!(audit["id"], 1);
            assert_eq!(audit["ownerGeneration"], 7);
            assert_eq!(audit["phase"], "complete");
            assert_eq!(audit["close"], original["close"]);
            assert_eq!(audit["priorCreate"], original["priorCreate"]);
            assert_eq!(settled["attempt"], target);
            assert_eq!(settled["create"]["ticket"], ticket);
            assert_eq!(settled["create"]["identity"], identity);
            assert_eq!(settled["create"], audit["launch"]);
            let current_owners = read_owners()?;
            assert_eq!(current_owners["other-scope"], owners["other-scope"]);
            assert!(run(old(), state)
                .unwrap_err()
                .is::<controller::ControllerError>());
            assert!(renew().unwrap_err().is::<controller::ControllerError>());
            let replay = || run(retry_request(dispatch, Some(new_token)), state);
            let receipt = replay()?;
            assert_eq!(receipt["result"], "accepted");
            assert_eq!(receipt["duplicate"], true);
            assert_eq!(receipt["audit"], 1);
            assert_eq!(receipt["ticket"], ticket);
            assert_eq!(replay()?, receipt);
            assert_eq!(messages::read_callback_record(&dir, dispatch)?, settled);
            assert_eq!(
                crate::bundled::lookup_assignment(state, dispatch, &pending["attempt"])?["ticket"],
                ticket
            );
        }
        Ok(())
    }
}
