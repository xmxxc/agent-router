use anyhow::{bail, Result};
use serde_json::json;
use std::{env, ffi::OsString, path::Path};
use wfc_legacy_state::{open_offline_copy, reclaim_preview, state_dir, status};

const USAGE: &str = "wfc-legacy-state status [--active | --dispatch ID]\nwfc-legacy-state reclaim [--dispatch ID]\nwfc-legacy-state migrate --offline-copy DIR";

fn run(args: Vec<OsString>) -> Result<()> {
    let words: Vec<_> = args.iter().map(|a| a.to_str()).collect();
    let view = match words.as_slice() {
        [Some("status")] => status(&state_dir()?, None, false)?,
        [Some("status"), Some("--active")] => status(&state_dir()?, None, true)?,
        [Some("status"), Some("--dispatch"), Some(id)] => status(&state_dir()?, Some(id), false)?,
        [Some("reclaim")] => reclaim_preview(&state_dir()?, None)?,
        [Some("reclaim"), Some("--dispatch"), Some(id)] => {
            reclaim_preview(&state_dir()?, Some(id))?
        }
        [Some("migrate"), Some("--offline-copy"), _] if !args[2].is_empty() => {
            let path = Path::new(&args[2]);
            let (_db, backup) = open_offline_copy(path)?;
            json!({"state":std::fs::canonicalize(path)?,"backup":backup})
        }
        [Some("--help" | "-h")] => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => bail!("{USAGE}"),
    };
    println!("{}", serde_json::to_string(&view)?);
    Ok(())
}

fn main() {
    if let Err(e) = run(env::args_os().skip(1).collect()) {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
