mod kernel;
mod workflow;

use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

const RESERVED_COMMANDS: &[&str] = &[
    "done", "ask", "reply", "continue", "nudge", "resume", "reclaim", "wait", "relay", "ack",
];

const UNEXPECTED_ARGUMENTS: &str = "wfc: unexpected arguments. See wfc --help";

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
            println!("{}", workflow::worker_owner::CLOSE_USAGE);
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
        Some("dispatch" | "status") if args == ["--help"] || args == ["-h"] => {
            println!(
                "{}",
                if command == "dispatch" {
                    workflow::DISPATCH_USAGE
                } else {
                    workflow::STATUS_USAGE
                }
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
