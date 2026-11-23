use std::process::Command;

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
    let build_id = option_env!("WFC_BUILD_ID").unwrap_or(env!("WFC_BUILT_AT"));
    assert!(version.contains(&format!("build {build_id})")));
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
