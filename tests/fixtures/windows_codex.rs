//! Local Codex substitute for the Windows public CLI integration test.
use std::{
    env,
    ffi::c_void,
    fs,
    io::{self, Write},
    path::PathBuf,
    process::Command,
    thread,
    time::Duration,
};

#[link(name = "kernel32")]
extern "system" {
    fn GetConsoleWindow() -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
    fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: &mut i32) -> i32;
}

fn setting(name: &str) -> String {
    env::var(name).unwrap()
}

fn main() {
    let args: Vec<_> = env::args().collect();
    match args[1].as_str() {
        "queue" => {
            if cfg!(fixture_decoy) {
                fs::write(setting("WFC_FIXTURE_DECOY"), "wrong executable").unwrap();
                std::process::exit(42);
            }
            assert_eq!(
                env::current_exe().unwrap().canonicalize().unwrap(),
                fs::canonicalize(setting("WFC_FIXTURE_EXPECTED_CODEX")).unwrap()
            );
            let mut in_job = 0;
            unsafe {
                assert!(GetConsoleWindow().is_null());
                assert_ne!(
                    IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job),
                    0
                );
            }
            assert_ne!(in_job, 0);
            assert_eq!(&args[2..4], &["--thread", &setting("CODEX_THREAD_ID")]);
            assert_eq!(args[4], "--message");
            assert_eq!(args[6], "--resume");
            let mut queue = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(setting("WFC_FIXTURE_QUEUE"))
                .unwrap();
            writeln!(queue, "{}", args[5]).unwrap();
            match env::var("WFC_FIXTURE_QUEUE_MODE").as_deref() {
                Ok("utf8") => {
                    eprintln!(
                        "Error: failed to start embedded app server: 拒绝访问。 (os error 5)"
                    );
                    eprintln!("fixture-private-receipt {}", args[5]);
                    std::process::exit(1);
                }
                Ok("invalid") => {
                    io::stderr()
                        .write_all(b"fixture-private-receipt \xff\n")
                        .unwrap();
                    std::process::exit(1);
                }
                _ => (),
            }
        }
        "raw" => (),
        "worker" => {
            assert!(args.iter().any(|arg| arg == "--no-alt-screen"));
            assert!(!args
                .iter()
                .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
            if args[2] == "fast" {
                std::process::exit(259);
            }
            let report = PathBuf::from(setting("WFC_REPORT"));
            if args[2] == "selected" {
                let exe = env::current_exe().unwrap();
                let staging = report.with_extension("tmp");
                fs::write(&staging, exe.to_str().unwrap()).unwrap();
                fs::rename(staging, &report).unwrap();
                return;
            }
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
