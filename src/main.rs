mod kernel;

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

const WORKFLOW_COMMANDS: &[&str] = &[
    "dispatch", "done", "ask", "reply", "continue", "nudge", "resume", "status", "reclaim", "wait",
    "relay", "ack",
];

fn main() -> anyhow::Result<ExitCode> {
    let mut args = env::args_os();
    let program = args.next().unwrap_or_default();
    let command = args.next().unwrap_or_else(|| {
        // Python's bundled backend starts a copy named wfc-kernel with no arguments.
        let companion = format!("wfc-kernel{}", env::consts::EXE_SUFFIX);
        if Path::new(&program)
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(OsStr::new(&companion)))
        {
            "--kernel".into()
        } else {
            "--help".into()
        }
    });
    let extra = args.next().is_some();
    match command.to_str() {
        Some("-h" | "--help") if !extra => {
            println!("Usage: wfc [--help | --version]");
            println!(
                "Workflow commands reserved for later builds: {}",
                WORKFLOW_COMMANDS.join(", ")
            );
        }
        Some("-V" | "--version") if !extra => {
            println!(
                "wfc {} (Rust, {}, {}, build {})",
                env!("CARGO_PKG_VERSION"),
                env!("WFC_TARGET"),
                env!("WFC_PROFILE"),
                option_env!("WFC_BUILD_ID").unwrap_or(env!("WFC_BUILT_AT"))
            );
        }
        Some("--kernel") if !extra => kernel::run()?,
        Some(name) if WORKFLOW_COMMANDS.contains(&name) => {
            eprintln!("wfc: '{name}' is unavailable in this Rust build");
            return Ok(ExitCode::from(2));
        }
        _ => {
            eprintln!("wfc: unexpected arguments. See wfc --help");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}
