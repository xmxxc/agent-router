use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let built_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    println!("cargo:rustc-env=WFC_BUILT_AT={}", built_at.as_nanos());
    println!("cargo:rustc-env=WFC_TARGET={}", env::var("TARGET").unwrap());
    println!(
        "cargo:rustc-env=WFC_PROFILE={}",
        env::var("PROFILE").unwrap()
    );
}
