//! Validates native command contract; these metadata checks do not claim boot.
use moruna_vmm::cli::{Command, parse};
#[test]
fn native_binary_has_no_linux_guest_or_oci_option_path() {
    for text in [
        "boot --image old --memory 512",
        "resize --cpus 2",
        "status --vsock 3",
        "stop --control old",
    ] {
        let args = text
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(parse(&args).is_err());
    }
    assert!(matches!(parse(&["--help".into()]).unwrap(), Command::Help));
}
