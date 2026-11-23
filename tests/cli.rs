use std::{env, fs, path::Path, process::Command};

#[test]
fn version_identifies_the_rust_build() {
    let output = Command::new(env!("CARGO_BIN_EXE_wfc"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let version = String::from_utf8(output.stdout).unwrap();
    assert!(version.starts_with(concat!("wfc ", env!("CARGO_PKG_VERSION"))));
    assert!(version.contains("Rust"));
    assert!(version.contains(env!("WFC_TARGET")));
    assert!(version.contains(env!("WFC_PROFILE")));
    let build_id = version
        .trim_end()
        .strip_suffix(')')
        .and_then(|line| line.split_once(", build "))
        .expect("expected version line ending in a build ID")
        .1;
    assert!(
        !build_id.trim().is_empty(),
        "missing build ID in {version:?}"
    );
    if let Some(expected) = option_env!("WFC_BUILD_ID").filter(|value| !value.trim().is_empty()) {
        assert_eq!(build_id, expected);
    } else {
        assert!(build_id.parse::<u128>().unwrap() > 0);
    }
}

#[test]
fn cli_and_kernel_have_separate_entry_points() {
    for args in [&[][..], &["--help"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_wfc"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .contains("dispatch"));
    }
    for args in [
        ["--kernel", "extra"],
        ["dispatch", "--host"],
        ["unknown", "argument"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_wfc"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn original_kernel_filename_starts_protocol_without_arguments() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("wfc-legacy-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let binary = dir.join(format!("herdr-kernel-prototype{}", env::consts::EXE_SUFFIX));
    fs::hard_link(env!("CARGO_BIN_EXE_wfc"), &binary).unwrap();
    let result = Command::new(&binary).output();
    fs::remove_dir_all(&dir).unwrap();

    let output = result.unwrap();
    assert!(output.status.success());
    let hello: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(hello["event"], "hello");
    assert_eq!(hello["protocol"], 1);
}
