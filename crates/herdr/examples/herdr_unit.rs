//! Backend contract driver. One private directory represents one test attempt.
//! The production router supplies its own owner/attempt transaction and input claim.
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use serde_json::{json, Value};
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;
    use wfc_herdr::{FailureKind, Handle, Launch, Mapping};

    let args: Vec<_> = std::env::args().collect();
    let [_, directory, operation] = args.as_slice() else {
        return Err(
            "usage: herdr_unit PRIVATE_ATTEMPT_DIRECTORY OPERATION < parameters.json".into(),
        );
    };
    let dir = Path::new(directory);
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
        return Err("expected a private owned attempt directory".into());
    }
    let lock = File::open(dir)?;
    lock.lock()?;
    let path = dir.join("unit.json");
    let mut state: Value = match File::open(&path) {
        Ok(file) => serde_json::from_reader(file)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({"audit":[]}),
        Err(e) => return Err(e.into()),
    };
    let save = |state: &Value| -> io::Result<()> {
        let tmp = dir.join(format!(".{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let result = (|| {
            serde_json::to_writer(&mut file, state)?;
            file.flush()?;
            file.sync_all()?;
            fs::rename(&tmp, &path)?;
            lock.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    };
    let input: Value = serde_json::from_reader(io::stdin().take(2 * 1024 * 1024 + 1))?;
    let value = if operation == "create" {
        if state.get("handle").is_some() {
            return Err("create already reserved".into());
        }
        let launch: Launch = serde_json::from_value(input["launch"].clone())?;
        let endpoint = input["endpoint"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| std::env::var("WFC_HERDR_ENDPOINT").ok())
            .ok_or("missing endpoint")?;
        Handle::create(
            Path::new(&endpoint),
            input["attempt"].as_str().ok_or("missing attempt")?,
            &launch,
            |h| {
                state["handle"] = json!(h);
                save(&state)
            },
        )
        .map(|_| json!({"result":"accepted"}))
    } else {
        let mut handle: Handle = serde_json::from_value(state["handle"].clone())?;
        match operation.as_str() {
            "inspect" => handle
                .inspect()
                .map(|alive| json!({"result":"accepted","alive":alive})),
            "read" => handle
                .read()
                .map(|screen| json!({"result":"accepted","screen":screen})),
            "interrupt" => handle.interrupt().map(|_| json!({"result":"accepted"})),
            "submit" => {
                let text = input["text"].as_str().ok_or("missing text")?;
                if state["pendingInput"] == true {
                    return Err("input still pending".into());
                }
                state["pendingInput"] = json!(true);
                save(&state)?;
                let result = handle.submit(text, input["host"] == "codex");
                if !matches!(result,Err(e) if e.kind == FailureKind::Uncertain) {
                    state["pendingInput"] = json!(false);
                    save(&state)?;
                }
                result.map(|_| json!({"result":"accepted"}))
            }
            "close" => handle
                .close(|h| {
                    state["handle"] = json!(h);
                    save(&state)
                })
                .map(|_| json!({"result":"completed"})),
            "herdr-reconcile" => {
                let mapping: Mapping = serde_json::from_value(input)?;
                match handle.reconcile(&mapping) {
                    Ok(binding) => {
                        state["handle"] = json!(binding.handle);
                        state["audit"]
                            .as_array_mut()
                            .ok_or("invalid audit")?
                            .push(binding.audit.clone());
                        save(&state)?;
                        Ok(json!({"result":"accepted","audit":binding.audit,"inputReplayed":false}))
                    }
                    Err(err) => Err(err),
                }
            }
            _ => return Err("unknown Herdr operation".into()),
        }
    };
    match value {
        Ok(data) => println!("{data}"),
        Err(error) => {
            println!("{}", json!(error));
            std::process::exit(1);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("this driver requires Linux");
    std::process::exit(1);
}
