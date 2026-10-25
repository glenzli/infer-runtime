use std::fs::{self, OpenOptions};

use super::*;
use crate::windows_security::tests::set_acl;

#[test]
fn raw_handles_require_private_single_link_files_and_exact_access_modes() {
    let root = tempfile::tempdir().unwrap();
    let input_path = root.path().join("input");
    fs::write(&input_path, b"sample").unwrap();
    set_acl(&input_path, "");
    let read_only = File::open(&input_path).unwrap();
    validate_file(&read_only, true).unwrap();
    let read_write = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&input_path)
        .unwrap();
    assert!(validate_file(&read_write, true).is_err());
    assert!(validate_file(&read_only, false).is_err());
    fs::hard_link(&input_path, root.path().join("alias")).unwrap();
    assert!(validate_file(&read_only, true).is_err());
    let output_path = root.path().join("output");
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .unwrap();
    set_acl(&output_path, "");
    validate_file(&output, false).unwrap();
    let append = OpenOptions::new().append(true).open(&output_path).unwrap();
    assert!(validate_file(&append, false).is_err());
    output.set_len(1).unwrap();
    assert!(validate_file(&output, false).is_err());
    output.set_len(0).unwrap();
    set_acl(&output_path, "(A;;GR;;;WD)");
    assert!(validate_file(&output, false).is_err());
}

#[test]
fn output_abort_truncates_partial_and_releases_exclusive_lock() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("partial");
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    output.try_lock().unwrap();
    let lease = OutputLease {
        file: output.try_clone().unwrap(),
        complete: false,
    };
    lease.file.set_len(4096).unwrap();
    drop(lease);
    assert_eq!(output.metadata().unwrap().len(), 0);
    let other = OpenOptions::new().write(true).open(&path).unwrap();
    other.try_lock().unwrap();
    other.unlock().unwrap();
}

#[test]
fn bridge_installation_rejects_option_injection_and_nonabsolute_code_paths() {
    assert!(
        RawFoundationWslBridge::new("Ubuntu-24.04", "/opt/infer/raw_foundation_wsl_bridge.py")
            .is_ok()
    );
    assert!(RawFoundationWslBridge::new("--exec", "/opt/infer/bridge.py").is_err());
    assert!(RawFoundationWslBridge::new("Ubuntu\n--root", "/opt/infer/bridge.py").is_err());
    assert!(RawFoundationWslBridge::new("Ubuntu", "relative.py").is_err());
    assert!(RawFoundationWslBridge::from_discovery_endpoint("wsl-stdio://Ubuntu-24.04/home/glen/.local/lib/infer-runtime/current/raw_foundation_wsl_bridge.py").is_ok());
    assert!(
        RawFoundationWslBridge::from_discovery_endpoint(
            "wsl-stdio://Ubuntu/home/glen/bridge.py?command=other"
        )
        .is_err()
    );
    assert!(
        RawFoundationWslBridge::from_discovery_endpoint("file:///home/glen/bridge.py").is_err()
    );
}
