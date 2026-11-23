#[cfg(target_os = "linux")]
mod host_launch;
mod kernel;
#[cfg(target_os = "linux")]
mod tmux;
mod workflow;

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;
use workflow::messages::{Confirm, Done, Wait, ACK_USAGE, DONE_USAGE, RESOLVE_USAGE, WAIT_USAGE};

const RESERVED_COMMANDS: &[&str] = &[
    "ask",
    "reply",
    "continue",
    "nudge",
    "resume",
    #[cfg(not(target_os = "linux"))]
    "reclaim",
    #[cfg(not(target_os = "linux"))]
    "relay",
];

const UNEXPECTED_ARGUMENTS: &str = "wfc: unexpected arguments. See wfc --help";
#[cfg(target_os = "linux")]
const RECLAIM_USAGE: &str = "wfc reclaim [--dispatch ID] (read-only preview of Python history)";

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
            #[cfg(target_os = "linux")]
            println!("{RECLAIM_USAGE}");
            #[cfg(target_os = "linux")]
            println!("{}", workflow::worker_owner::CLOSE_USAGE);
            #[cfg(target_os = "linux")]
            println!("{}", workflow::herdr::USAGE);
            println!("{DONE_USAGE}");
            println!("{WAIT_USAGE}");
            #[cfg(target_os = "linux")]
            println!("{}", workflow::messages::relay::USAGE);
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
        #[cfg(target_os = "linux")]
        Some("herdr-reconcile") => {
            use workflow::herdr::{Reconcile, USAGE};
            if args == ["--help"] || args == ["-h"] {
                println!("{USAGE}");
            } else {
                let request = match Reconcile::parse(&args) {
                    Ok(request) => request,
                    Err(e) => {
                        eprintln!("wfc: {e}. {USAGE}");
                        return Ok(ExitCode::from(2));
                    }
                };
                println!("{}", request.run()?);
            }
        }
        #[cfg(target_os = "linux")]
        Some("--herdr-backend") if args.is_empty() => {
            let request = serde_json::from_reader(std::io::stdin().lock())?;
            let reply = workflow::herdr::submit(request)?;
            println!("{reply}");
            if reply["result"] != "accepted" {
                return Ok(ExitCode::FAILURE);
            }
        }
        #[cfg(target_os = "linux")]
        Some("--herdr-host") if args.len() == 2 => {
            let spec = host_launch::spec_path(Path::new(&args[0]), Path::new(&args[1]))?;
            return host_launch::supervise(&spec);
        }
        #[cfg(target_os = "linux")]
        Some("--tmux-backend") if args.is_empty() => {
            let request = serde_json::from_reader(std::io::stdin().lock())?;
            println!("{}", tmux::execute(&workflow::state_dir()?, request)?);
        }
        #[cfg(target_os = "linux")]
        Some("--tmux-attach") if args.len() == 4 => {
            tmux::attach(
                Path::new(&args[0]),
                args[1].to_str().unwrap_or_default(),
                Path::new(&args[2]),
                args[3].to_str().unwrap_or_default(),
            )?;
        }
        #[cfg(target_os = "linux")]
        Some("--tmux-record-session") if args.len() == 8 => {
            tmux::record_created_session(&args)?;
        }
        #[cfg(target_os = "linux")]
        Some("--tmux-host") if args.len() == 1 => {
            return tmux::launch::run_host(Path::new(&args[0]));
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
        #[cfg(target_os = "linux")]
        Some("--relay-queue") if args.len() == 4 => {
            return workflow::messages::relay::queue_owner::run(
                &args[0], &args[1], &args[2], &args[3],
            );
        }
        #[cfg(target_os = "linux")]
        Some("--worker-owner") => workflow::worker_owner::run(&args)?,
        #[cfg(target_os = "linux")]
        Some("close") => match args.as_slice() {
            [help] if help == "--help" || help == "-h" => {
                println!("{}", workflow::worker_owner::CLOSE_USAGE)
            }
            [id] if !id.as_encoded_bytes().starts_with(b"-") => {
                println!("{}", workflow::worker_owner::close(id)?)
            }
            _ => {
                eprintln!("wfc: {}", workflow::worker_owner::CLOSE_USAGE);
                return Ok(ExitCode::from(2));
            }
        },
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
            let (id, active) = match args.as_slice() {
                [] => (None, false),
                [flag] if flag == "--active" => (None, true),
                [flag, id] if flag == "--dispatch" => (Some(id.as_os_str()), false),
                _ => {
                    eprintln!("wfc: {}", workflow::STATUS_USAGE);
                    return Ok(ExitCode::from(2));
                }
            };
            println!("{}", workflow::status(id, active)?);
        }
        #[cfg(target_os = "linux")]
        Some("reclaim") => {
            let dispatch = match args.as_slice() {
                [] => None,
                [flag] if flag == "--help" || flag == "-h" => {
                    println!("{RECLAIM_USAGE}");
                    return Ok(ExitCode::SUCCESS);
                }
                [flag, id]
                    if flag == "--dispatch"
                        && !id.is_empty()
                        && !id.as_encoded_bytes().starts_with(b"-") =>
                {
                    Some(
                        id.to_str()
                            .ok_or_else(|| anyhow::anyhow!("invalid dispatch ID"))?,
                    )
                }
                _ => {
                    eprintln!("wfc: {RECLAIM_USAGE}");
                    return Ok(ExitCode::from(2));
                }
            };
            let state = wfc_legacy_state::state_dir()?;
            println!("{}", wfc_legacy_state::reclaim_preview(&state, dispatch)?);
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
        #[cfg(target_os = "linux")]
        Some("relay") => {
            use workflow::messages::relay::{Relay, USAGE};
            if args == ["--help"] || args == ["-h"] {
                println!("{USAGE}");
            } else {
                let relay = match Relay::parse(&args) {
                    Ok(relay) => relay,
                    Err(err) => {
                        eprintln!("wfc: {err}. {USAGE}");
                        return Ok(ExitCode::from(2));
                    }
                };
                relay.run()?;
            }
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
            eprintln!("{UNEXPECTED_ARGUMENTS}");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}
