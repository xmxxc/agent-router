mod kernel;
mod workflow;

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;
use workflow::messages::{Confirm, Done, Wait, ACK_USAGE, DONE_USAGE, RESOLVE_USAGE, WAIT_USAGE};

const RESERVED_COMMANDS: &[&str] = &[
    "ask", "reply", "continue", "nudge", "resume", "reclaim", "relay",
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
    let args: Vec<_> = args.collect();
    match command.to_str() {
        Some("-h" | "--help") if args.is_empty() => {
            println!("Usage: wfc [--help | --version]");
            println!("{}", workflow::DISPATCH_USAGE);
            println!("{}", workflow::STATUS_USAGE);
            println!("{DONE_USAGE}");
            println!("{WAIT_USAGE}");
            println!("{ACK_USAGE}");
            println!("{RESOLVE_USAGE}");
            println!(
                "Workflow commands reserved for later builds: {}",
                RESERVED_COMMANDS.join(", ")
            );
        }
        Some("-V" | "--version") if args.is_empty() => {
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
        Some("--kernel") if args.is_empty() => kernel::run()?,
        Some(name @ ("dispatch" | "status" | "done" | "wait" | "ack" | "resolve"))
            if args == ["--help"]
                || (matches!(name, "done" | "wait" | "ack" | "resolve") && args == ["-h"]) =>
        {
            let usage = match name {
                "dispatch" => workflow::DISPATCH_USAGE,
                "status" => workflow::STATUS_USAGE,
                "done" => DONE_USAGE,
                "wait" => WAIT_USAGE,
                "ack" => ACK_USAGE,
                _ => RESOLVE_USAGE,
            };
            println!("{usage}");
        }
        Some("dispatch") => {
            let request = match workflow::Dispatch::parse(&args) {
                Ok(request) => request,
                Err(err) => {
                    eprintln!("wfc: {err}. {}", workflow::DISPATCH_USAGE);
                    return Ok(ExitCode::from(2));
                }
            };
            println!("{}", request.run()?);
        }
        Some("status") => {
            let id = match args.as_slice() {
                [] => None,
                [flag, id] if flag == "--dispatch" => Some(id.as_os_str()),
                _ => {
                    eprintln!("wfc: {}", workflow::STATUS_USAGE);
                    return Ok(ExitCode::from(2));
                }
            };
            println!("{}", workflow::status(id)?);
        }
        Some("done") => {
            let request = match Done::parse(&args) {
                Ok(request) => request,
                Err(err) => {
                    eprintln!("wfc: {err}. {DONE_USAGE}");
                    return Ok(ExitCode::from(2));
                }
            };
            println!("{}", request.run()?);
        }
        Some(name @ ("ack" | "resolve")) => {
            let request = match Confirm::parse(name, &args) {
                Ok(request) => request,
                Err(err) => {
                    let usage = if name == "ack" {
                        ACK_USAGE
                    } else {
                        RESOLVE_USAGE
                    };
                    eprintln!("wfc: {err}. {usage}");
                    return Ok(ExitCode::from(2));
                }
            };
            println!("{}", request.run()?);
        }
        Some("wait") => {
            let request = match Wait::parse(&args) {
                Ok(request) => request,
                Err(err) => {
                    eprintln!("wfc: {err}. {WAIT_USAGE}");
                    return Ok(ExitCode::from(2));
                }
            };
            let messages = request.run()?;
            if messages.is_empty() {
                println!("{}", serde_json::json!({"messages": [], "timedOut": true}));
                return Ok(ExitCode::from(3));
            }
            println!("{}", serde_json::json!({"messages": messages}));
        }
        Some(name) if RESERVED_COMMANDS.contains(&name) => {
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
