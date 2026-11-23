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
        // Python launches both the bundled companion and the original kernel by path alone.
        let name = Path::new(&program).file_name().unwrap_or_default();
        let suffix = env::consts::EXE_SUFFIX;
        if ["wfc-kernel", "herdr-kernel-prototype"]
            .iter()
            .any(|stem| name.eq_ignore_ascii_case(OsStr::new(&format!("{stem}{suffix}"))))
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
                option_env!("WFC_BUILD_ID")
                    .filter(|id| !id.trim().is_empty())
                    .unwrap_or(env!("WFC_BUILT_AT"))
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
