//! Local Codex substitute for the Windows public CLI integration test.
use std::{env, fs, io::Write, path::PathBuf, process::Command, thread, time::Duration};

fn setting(name: &str) -> String {
    env::var(name).unwrap()
}

fn main() {
    let args: Vec<_> = env::args().collect();
    match args[1].as_str() {
        "queue" => {
            assert_eq!(&args[2..4], &["--thread", &setting("CODEX_THREAD_ID")]);
            assert_eq!(args[4], "--message");
            assert_eq!(args[6], "--resume");
            let mut queue = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(setting("WFC_FIXTURE_QUEUE"))
                .unwrap();
            writeln!(queue, "{}", args[5]).unwrap();
        }
        "worker" => {
            assert!(args.iter().any(|arg| arg == "--no-alt-screen"));
            assert!(!args
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
            if args[2] == "fast" {
                std::process::exit(259);
            }
            let report = PathBuf::from(setting("WFC_REPORT"));
            let mut child = Command::new(env::current_exe().unwrap())
                .arg("leaf")
                .spawn()
                .unwrap();
            fs::write(report.with_extension("pid"), child.id().to_string()).unwrap();
            thread::sleep(Duration::from_millis(1400));
            let cwd = env::current_dir().unwrap();
            fs::write(&report, format!("window fixture ran in {}", cwd.display())).unwrap();
            for _ in 0..2 {
                let done = Command::new(setting("WFC_BIN"))
                    .arg("done")
                    .args(["--attempt", &setting("WFC_ATTEMPT")])
                    .arg("--report")
                    .arg(&report)
                    .output()
                    .unwrap();
                assert!(
                    done.status.success(),
                    "{}",
                    String::from_utf8_lossy(&done.stderr)
                );
            }
            if args[2] == "hold" {
                child.wait().unwrap();
            }
        }
        "leaf" => thread::sleep(Duration::from_secs(45)),
        _ => panic!("unexpected command"),
    }
}
