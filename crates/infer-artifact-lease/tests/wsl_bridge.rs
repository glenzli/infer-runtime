#![cfg(target_os = "linux")]

use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use infer_artifact_lease::{
    ArtifactLeaseRegistry, ArtifactLeaseUnixService, copy_striped_and_hash,
};
use serde_json::{Value, json};

const SCHEMA: &str = "infer.raw-foundation.wsl-stdio@20261025.1";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn wsl_stream_uses_real_scm_leases_checks_artifact_and_cleans_on_eof() {
    for collect in [true, false] {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry = Arc::new(
            ArtifactLeaseRegistry::new(unsafe { libc::geteuid() }, "wsl-test-generation").unwrap(),
        );
        let socket = root.path().join("lease.sock");
        let _service = ArtifactLeaseUnixService::start(&socket, Arc::clone(&registry)).unwrap();
        let ticket = registry
            .issue_ticket("shadow", "job", "wsl-test-generation", 5_000, now())
            .unwrap();
        let mut child = Command::new("python3")
            .args([
                "-I",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tools/raw_foundation_wsl_bridge.py"
                ),
            ])
            .env("TMPDIR", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let samples: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
        writeln!(stdin, "{}", json!({"schema":SCHEMA,"endpoint":socket,"ticket_id":ticket.ticket_id,"sample_bytes":samples.len(),"expires_at_unix_ms":ticket.expires_at_unix_ms})).unwrap();
        stdin.write_all(&samples).unwrap();
        stdin.flush().unwrap();
        let mut response = String::new();
        stdout.read_line(&mut response).unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["schema"], SCHEMA);
        let lease_id = response["lease_id"].as_str().unwrap();
        if collect {
            writeln!(stdin, "{}", json!({"schema":SCHEMA,"operation":"execute"})).unwrap();
            let lease = registry
                .consume(lease_id, "shadow", "job", "wsl-test-generation", now())
                .unwrap();
            let receipt = copy_striped_and_hash(lease).unwrap();
            assert_eq!(receipt.maximum_explicit_buffer_bytes, 65536);
            writeln!(stdin, "{}", json!({"schema":SCHEMA,"operation":"collect","artifact_file_bytes":receipt.bytes_written,"artifact_file_sha256":receipt.sha256})).unwrap();
            stdin.flush().unwrap();
            let mut header = String::new();
            stdout.read_line(&mut header).unwrap();
            let header: Value = serde_json::from_str(&header).unwrap();
            assert_eq!(header["artifact_file_bytes"], samples.len());
            let mut artifact = Vec::new();
            stdout.read_to_end(&mut artifact).unwrap();
            assert_eq!(artifact, samples);
        } else {
            registry
                .revoke_scope("shadow", "job", "wsl-test-generation")
                .unwrap();
        }
        drop(stdin);
        assert_eq!(child.wait().unwrap().success(), collect);
        assert_eq!(registry.active_counts().unwrap(), (0, 0));
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "private staging must be removed; only the service socket remains"
        );
    }
}
