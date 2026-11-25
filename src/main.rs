#[cfg(target_os = "linux")]
mod bundled;
#[cfg(target_os = "linux")]
mod host_launch;
mod kernel;
mod workflow;

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;
use workflow::controller::{self, ControllerError, Owner};
use workflow::messages::{
    Ask, Confirm, Done, Wait, ACK_USAGE, ASK_USAGE, DONE_USAGE, RESOLVE_USAGE, WAIT_USAGE,
};
#[cfg(target_os = "linux")]
use workflow::messages::{Reply, REPLY_USAGE};

const RESERVED_COMMANDS: &[&str] = &[
    #[cfg(not(target_os = "linux"))]
    "reply",
    #[cfg(not(target_os = "linux"))]
    "continue",
    #[cfg(not(target_os = "linux"))]
    "nudge",
    #[cfg(not(target_os = "linux"))]
    "resume",
    #[cfg(not(target_os = "linux"))]
    "handoff",
    #[cfg(not(target_os = "linux"))]
    "reclaim",
    #[cfg(not(target_os = "linux"))]
    "relay",
];

const UNEXPECTED_ARGUMENTS: &str = "wfc: unexpected arguments. See wfc --help";

fn main() -> anyhow::Result<ExitCode> {
    match run() {
        Err(err) if err.is::<ControllerError>() => {
            eprintln!("{}", err.downcast_ref::<ControllerError>().unwrap());
            Ok(ExitCode::FAILURE)
        }
        result => result,
    }
}

fn run() -> anyhow::Result<ExitCode> {
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
            println!("{}", workflow::reclaim::USAGE);
            #[cfg(target_os = "linux")]
            println!("{}", workflow::legacy_import::USAGE);
            #[cfg(target_os = "linux")]
            println!("{}", workflow::worker_owner::CLOSE_USAGE);
            #[cfg(target_os = "linux")]
            for usage in [
                workflow::input::CONTINUE_USAGE,
                workflow::input::NUDGE_USAGE,
            ] {
                println!("{usage}");
            }
            #[cfg(target_os = "linux")]
            println!("{}", workflow::herdr::USAGE);
            #[cfg(target_os = "linux")]
            println!("{}", workflow::resume::USAGE);
            #[cfg(target_os = "linux")]
            println!("{}", workflow::handoff::USAGE);
            println!("{DONE_USAGE}");
            println!("{ASK_USAGE}");
            #[cfg(target_os = "linux")]
            println!("{REPLY_USAGE}");
            println!("{WAIT_USAGE}");
            #[cfg(target_os = "linux")]
            println!("{}", workflow::messages::relay::USAGE);
            println!("{ACK_USAGE}");
            println!("{RESOLVE_USAGE}");
            println!("{}", controller::USAGE);
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
        Some("--claude-session-start") if args.is_empty() => {
            workflow::host::claude_session_start()?
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
        Some(stage @ ("--bundled" | "--bundled-guardian" | "--bundled-runtime"))
            if args.len() <= 1 =>
        {
            return Ok(ExitCode::from(bundled::run(
                stage,
                args.first().map(Path::new),
            )?));
        }
        #[cfg(target_os = "linux")]
        Some("--bundled-user-guardian") if args.is_empty() => {
            return Ok(ExitCode::from(bundled::run(
                "--bundled-user-guardian",
                None,
            )?));
        }
        Some("--kernel") if args.is_empty() => kernel::run()?,
        Some(name @ ("dispatch" | "status" | "done" | "ask" | "wait" | "ack" | "resolve"))
            if args == ["--help"]
                || (matches!(name, "done" | "ask" | "wait" | "ack" | "resolve")
                    && args == ["-h"]) =>
        {
            let usage = match name {
                "dispatch" => workflow::DISPATCH_USAGE,
                "status" => workflow::STATUS_USAGE,
                "done" => DONE_USAGE,
                "ask" => ASK_USAGE,
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
        Some(name @ ("continue" | "nudge")) => {
            let usage = workflow::input::usage(name);
            if args == ["--help"] || args == ["-h"] {
                println!("{usage}");
            } else {
                let request = match workflow::input::Request::parse(name, &args) {
                    Ok(request) => request,
                    Err(err) => {
                        eprintln!("wfc: {err}. {usage}");
                        return Ok(ExitCode::from(2));
                    }
                };
                controller::require_controller(name)?;
                println!("{}", request.run()?);
            }
        }
        #[cfg(target_os = "linux")]
        Some("close") => {
            use workflow::worker_owner::{close, parse_close, CLOSE_USAGE};
            if args == ["--help"] || args == ["-h"] {
                println!("{CLOSE_USAGE}");
            } else {
                let (id, token) = match parse_close(&args) {
                    Ok(request) => request,
                    Err(err) => {
                        eprintln!("wfc: {err}. {CLOSE_USAGE}");
                        return Ok(ExitCode::from(2));
                    }
                };
                controller::require_controller("close")?;
                println!("{}", close(id, token)?);
            }
        }
        Some(name) if name == "owner" || controller::ALIASES.contains(&name) => {
            let (action, options) = if name == "owner" {
                match args.split_first() {
                    Some((action, rest)) => (action.to_str().unwrap_or_default(), rest),
                    None => ("", args.as_slice()),
                }
            } else {
                (name, args.as_slice())
            };
            if args == ["--help"]
                || args == ["-h"]
                || (controller::ALIASES.contains(&action)
                    && (options == ["--help"] || options == ["-h"]))
            {
                println!("{}", controller::USAGE);
            } else {
                if !controller::ALIASES.contains(&action) {
                    eprintln!("wfc: {}", controller::USAGE);
                    return Ok(ExitCode::from(2));
                }
                let request = match Owner::parse(action, options) {
                    Ok(request) => request,
                    Err(err) => {
                        eprintln!("wfc: {err}. {}", controller::USAGE);
                        return Ok(ExitCode::from(2));
                    }
                };
                controller::require_controller(name)?;
                println!("{}", request.run()?);
            }
        }
        #[cfg(target_os = "linux")]
        Some("resume") => {
            use workflow::resume::{Resume, USAGE};
            if args == ["--help"] || args == ["-h"] {
                println!("{USAGE}");
            } else {
                let request = match Resume::parse(&args) {
                    Ok(request) => request,
                    Err(err) => {
                        eprintln!("wfc: {err}. {USAGE}");
                        return Ok(ExitCode::from(2));
                    }
                };
                let reply = request.run()?;
                if reply["result"] != "accepted" {
                    eprintln!("{reply}");
                    return Ok(ExitCode::FAILURE);
                }
                println!("{reply}");
            }
        }
        #[cfg(target_os = "linux")]
        Some("handoff") => {
            use workflow::handoff::{Handoff, USAGE};
            if args == ["--help"] || args == ["-h"] {
                println!("{USAGE}");
            } else {
                let request = match Handoff::parse(&args) {
                    Ok(request) => request,
                    Err(err) => {
                        eprintln!("wfc: {err}. {USAGE}");
                        return Ok(ExitCode::from(2));
                    }
                };
                let reply = request.run()?;
                if reply["result"] == "refused" {
                    eprintln!("{reply}");
                    return Ok(ExitCode::FAILURE);
                }
                println!("{reply}");
            }
        }
        Some("dispatch") => {
            let request = match workflow::Dispatch::parse(&args) {
                Ok(request) => request,
                Err(err) => {
                    eprintln!("wfc: {err}. {}", workflow::DISPATCH_USAGE);
                    return Ok(ExitCode::from(2));
                }
            };
            controller::require_controller("dispatch")?;
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
            controller::require_controller("status")?;
            println!("{}", workflow::status(id, active)?);
        }
        #[cfg(target_os = "linux")]
        Some("migrate-legacy") => {
            if args == ["--help"] || args == ["-h"] {
                println!("{}", workflow::legacy_import::USAGE);
            } else {
                controller::require_controller("migrate-legacy")?;
                println!("{}", workflow::legacy_import::run(&args)?);
            }
        }
        #[cfg(target_os = "linux")]
        Some("reclaim") => {
            use workflow::reclaim::{Reclaim, USAGE};
            if args == ["--help"] || args == ["-h"] {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            let request = match Reclaim::parse(&args) {
                Ok(request) => request,
                Err(error) => {
                    eprintln!("wfc: {error}. {USAGE}");
                    return Ok(ExitCode::from(2));
                }
            };
            controller::require_controller("reclaim")?;
            return request.run();
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
        Some("ask") => {
            let request = match Ask::parse(&args) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    println!("{ASK_USAGE}");
                    return Ok(ExitCode::SUCCESS);
                }
                Err(err) => {
                    eprintln!("wfc: {err}. {ASK_USAGE}");
                    return Ok(ExitCode::from(2));
                }
            };
            println!("{}", request.run()?);
        }
        #[cfg(target_os = "linux")]
        Some("reply") => {
            if args == ["--help"] || args == ["-h"] {
                println!("{REPLY_USAGE}");
            } else {
                let request = match Reply::parse(&args) {
                    Ok(request) => request,
                    Err(err) => {
                        eprintln!("wfc: {err}. {REPLY_USAGE}");
                        return Ok(ExitCode::from(2));
                    }
                };
                controller::require_controller("reply")?;
                println!("{}", request.run()?);
            }
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
            controller::require_controller(name)?;
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
                controller::require_controller("relay")?;
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
            controller::require_controller("wait")?;
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
