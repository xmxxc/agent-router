#![cfg(target_os = "linux")]

use std::os::unix::{fs::PermissionsExt, net::UnixListener};
use wfc_herdr::{FailureKind, Handle, Launch};

#[test]
fn unsafe_socket_never_receives_a_request() {
    let path = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let result = Handle::create(
        &path,
        "attempt",
        &Launch {
            cwd: "/tmp".into(),
            label: "unit".into(),
            command: vec!["true".into()],
        },
        |_| Ok(()),
    );
    std::fs::remove_file(path).unwrap();
    assert_eq!(result.unwrap_err().kind, FailureKind::Refused);
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
