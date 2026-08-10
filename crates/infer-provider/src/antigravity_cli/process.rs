use std::{
    collections::BTreeSet,
    io::Read as _,
    path::Path,
    process::{ExitStatus, Stdio},
};

use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

use crate::{ProviderError, ProviderFailureKind};

const MAX_STDERR_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    diagnostic_modules: Vec<String>,
    log_tags: Vec<&'static str>,
    #[cfg(test)]
    diagnostic_excerpt: Vec<String>,
}

pub(super) async fn run(
    command: &str,
    base_args: &[String],
    invocation_args: &[String],
    max_stdout_bytes: usize,
) -> Result<ProcessOutput, ProviderError> {
    let workspace = TempDir::new()?;
    run_at(
        command,
        base_args,
        invocation_args,
        max_stdout_bytes,
        workspace.path(),
        true,
    )
    .await
}

pub(super) async fn run_in_user_session(
    command: &str,
    base_args: &[String],
    invocation_args: &[String],
    max_stdout_bytes: usize,
    workspace: &Path,
) -> Result<ProcessOutput, ProviderError> {
    run_at(
        command,
        base_args,
        invocation_args,
        max_stdout_bytes,
        workspace,
        true,
    )
    .await
}

async fn run_at(
    command: &str,
    base_args: &[String],
    invocation_args: &[String],
    max_stdout_bytes: usize,
    workspace: &Path,
    inherit_user_home: bool,
) -> Result<ProcessOutput, ProviderError> {
    let mut command_builder = Command::new(command);
    command_builder.env_clear();
    for name in [
        "PATH", "TMPDIR", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "SHELL",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command_builder.env(name, value);
        }
    }
    if inherit_user_home && let Some(home) = std::env::var_os("HOME") {
        command_builder.env("HOME", home);
    }
    let log_path = workspace.join("antigravity.log");
    let mut child = command_builder
        .args(base_args)
        // Keep this Attempt's explicit diagnostics inside the ephemeral
        // workspace even though authentication remains owned by the user's
        // installed CLI session.
        .arg("--log-file")
        .arg(&log_path)
        .args(invocation_args)
        .current_dir(workspace)
        .env("AGY_CLI_HIDE_ACCOUNT_INFO", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::Protocol("Antigravity stdout was not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProviderError::Protocol("Antigravity stderr was not piped".into()))?;
    let (stdout, stderr, status) = tokio::try_join!(
        drain_capped(stdout, max_stdout_bytes),
        drain_capped(stderr, MAX_STDERR_BYTES),
        child.wait(),
    )?;
    if stdout.truncated {
        return Err(ProviderError::Classified {
            kind: ProviderFailureKind::Protocol,
            message: "Antigravity stdout exceeded the bridge limit".into(),
        });
    }
    let log_text = read_log(&log_path);
    Ok(ProcessOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        diagnostic_modules: log_modules(&log_text),
        log_tags: diagnostic_tags_for_text(&log_text),
        #[cfg(test)]
        diagnostic_excerpt: sanitized_error_excerpt(&log_text),
    })
}

#[cfg(test)]
fn sanitized_error_excerpt(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| {
            let lowered = line.to_ascii_lowercase();
            lowered.contains("error") || lowered.contains("failed") || lowered.contains("fatal")
        })
        .rev()
        .take(3)
        .map(|line| sanitize_line(line.split_once("] ").map_or(line, |(_, message)| message)))
        .collect()
}

#[cfg(test)]
fn sanitize_line(line: &str) -> String {
    let mut output = String::new();
    let mut quoted = None;
    for token in line.split_whitespace() {
        if output.len() >= 240 {
            break;
        }
        let replacement = if quoted.is_some() {
            if token.ends_with(quoted.expect("quote is present")) {
                quoted = None;
            }
            None
        } else if token.starts_with('"') || token.starts_with('\'') {
            let quote = token.as_bytes()[0] as char;
            if !token[1..].contains(quote) {
                quoted = Some(quote);
            }
            Some("<value>")
        } else if token.starts_with("http://") || token.starts_with("https://") {
            Some("<url>")
        } else if token.starts_with('/') || token.contains('@') {
            Some("<private>")
        } else if token.len() > 40 {
            Some("<value>")
        } else {
            Some(token)
        };
        if let Some(replacement) = replacement {
            if !output.is_empty() {
                output.push(' ');
            }
            output.push_str(replacement);
        }
    }
    output.replace("bridge-ok", "<prompt>")
}

fn read_log(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut text = String::new();
    if file.take(256 * 1024).read_to_string(&mut text).is_err() {
        return String::new();
    }
    text
}

fn log_modules(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let prefix = line.split_once("] ")?.0;
            let location = prefix.split_ascii_whitespace().last()?;
            let (file, line) = location.rsplit_once(':')?;
            line.bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| Path::new(file).file_name()?.to_str().map(str::to_owned))?
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(16)
        .collect()
}

struct CappedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn drain_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<CappedBytes> {
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        if count <= remaining {
            bytes.extend_from_slice(&chunk[..count]);
        } else {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
        }
    }
    Ok(CappedBytes { bytes, truncated })
}

pub(super) fn classify_failed_process(output: &ProcessOutput) -> ProviderError {
    let kind = diagnostic_kind(output).unwrap_or(ProviderFailureKind::Unavailable);
    let summary = diagnostic_tags(output);
    let exit_code = output.status.code().unwrap_or(-1);
    let modules = output.diagnostic_modules.join(",");
    let log_tags = output.log_tags.join(",");
    #[cfg(test)]
    let excerpt = output.diagnostic_excerpt.join(" | ");
    let message = if summary.is_empty() {
        if modules.is_empty() && log_tags.is_empty() {
            format!("Antigravity CLI exited with {kind:?} (status={exit_code})")
        } else {
            format!(
                "Antigravity CLI exited with {kind:?} (status={exit_code};log={log_tags};modules={modules})"
            )
        }
    } else {
        format!(
            "Antigravity CLI exited with {kind:?} (status={exit_code};{};modules={modules})",
            summary.join(","),
        )
    };
    #[cfg(test)]
    let message = if excerpt.is_empty() {
        message
    } else {
        format!("{message} [sanitized={excerpt}]")
    };
    ProviderError::Classified {
        kind,
        // Never include stdout/stderr: both may contain account data, prompt
        // fragments, model output, or an OAuth URL.
        message,
    }
}

fn diagnostic_tags(output: &ProcessOutput) -> Vec<&'static str> {
    let mut diagnostic = String::from_utf8_lossy(&output.stderr).to_lowercase();
    diagnostic.push_str(&String::from_utf8_lossy(&output.stdout).to_lowercase());
    diagnostic_tags_for_text(&diagnostic)
}

fn diagnostic_tags_for_text(diagnostic: &str) -> Vec<&'static str> {
    [
        (
            "auth",
            [
                "sign in",
                "authentication",
                "oauth",
                "credential",
                "keyring",
            ]
            .as_slice(),
        ),
        (
            "settings",
            ["setting", "configuration", "config"].as_slice(),
        ),
        ("permission", ["permission", "denied"].as_slice()),
        ("project", ["project", "workspace"].as_slice()),
        ("network", ["network", "connection", "connect"].as_slice()),
        (
            "argument",
            ["unknown flag", "flag provided", "needs an argument"].as_slice(),
        ),
        (
            "missing",
            ["no such file", "not found", "missing"].as_slice(),
        ),
        ("invalid", ["invalid", "malformed"].as_slice()),
        ("panic", ["panic", "fatal"].as_slice()),
        ("onboarding", ["onboarding", "first run"].as_slice()),
        ("trust", ["trusted", "trust"].as_slice()),
        ("account", ["account", "profile"].as_slice()),
        ("storage", ["storage", "data dir", "directory"].as_slice()),
        ("json", ["json", "unmarshal", "decode"].as_slice()),
        ("parse", ["parse", "syntax"].as_slice()),
        ("resolve", ["resolve", "resolver"].as_slice()),
        (
            "initialize",
            ["initialize", "initialization", "startup"].as_slice(),
        ),
        ("color", ["color scheme", "colorscheme"].as_slice()),
        ("telemetry", ["telemetry", "analytics"].as_slice()),
        ("model", ["model", "gemini"].as_slice()),
        ("update", ["update", "version"].as_slice()),
        ("executable", ["executable", "binary"].as_slice()),
        ("agentapi", ["agentapi", "agent api"].as_slice()),
        (
            "default_project",
            ["default project", "project id"].as_slice(),
        ),
        ("state", ["state", "jetski"].as_slice()),
        ("certificate", ["certificate", "tls"].as_slice()),
        ("process", ["process", "subprocess", "child"].as_slice()),
        ("empty", ["empty", "zero"].as_slice()),
        ("failed", ["failed", "failure", "error"].as_slice()),
    ]
    .into_iter()
    .filter_map(|(tag, needles)| {
        needles
            .iter()
            .any(|needle| diagnostic.contains(needle))
            .then_some(tag)
    })
    .collect()
}

pub(super) fn reported_process_failure(output: &ProcessOutput) -> Option<ProviderError> {
    diagnostic_kind(output).map(|kind| ProviderError::Classified {
        kind,
        message: format!("Antigravity CLI reported {kind:?}"),
    })
}

fn diagnostic_kind(output: &ProcessOutput) -> Option<ProviderFailureKind> {
    let mut diagnostic = String::from_utf8_lossy(&output.stderr).to_lowercase();
    diagnostic.push_str(&String::from_utf8_lossy(&output.stdout).to_lowercase());
    if diagnostic.contains("sign in")
        || diagnostic.contains("authentication required")
        || diagnostic.contains("not authenticated")
    {
        Some(ProviderFailureKind::Authentication)
    } else if diagnostic.contains("rate limit")
        || diagnostic.contains("quota exceeded")
        || diagnostic.contains("usage limit")
    {
        Some(ProviderFailureKind::RateLimited)
    } else if diagnostic.contains("timed out") || diagnostic.contains("timeout") {
        Some(ProviderFailureKind::Timeout)
    } else if diagnostic.contains("cannot be resolved")
        || diagnostic.contains("unknown model")
        || diagnostic.contains("invalid model")
    {
        Some(ProviderFailureKind::InvalidRequest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn bounds_stdout_while_draining_the_child() {
        let output = run(
            "/bin/sh",
            &["-c".into(), "printf 123456789".into(), "test".into()],
            &[],
            4,
        )
        .await
        .unwrap_err();
        assert_eq!(output.kind(), ProviderFailureKind::Protocol);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_environment_drops_build_and_runtime_secrets() {
        let output = run(
            "/bin/sh",
            &["-c".into(), "/usr/bin/env".into(), "test".into()],
            &[],
            64 * 1024,
        )
        .await
        .unwrap();
        let environment = String::from_utf8(output.stdout).unwrap();
        assert!(!environment.contains("CARGO_MANIFEST_DIR="));
        assert!(!environment.contains("RUSTUP_TOOLCHAIN="));
        assert!(environment.contains("AGY_CLI_HIDE_ACCOUNT_INFO=1"));
        assert!(environment.contains("HOME="));
    }

    #[cfg(unix)]
    #[test]
    fn classifies_auth_without_exposing_the_diagnostic() {
        use std::os::unix::process::ExitStatusExt;

        let output = ProcessOutput {
            status: ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"Please sign in at https://example.test/private".to_vec(),
            diagnostic_modules: Vec::new(),
            log_tags: Vec::new(),
            diagnostic_excerpt: Vec::new(),
        };
        let error = classify_failed_process(&output);
        assert_eq!(error.kind(), ProviderFailureKind::Authentication);
        assert!(!error.to_string().contains("example.test"));
        assert_eq!(
            reported_process_failure(&output).unwrap().kind(),
            ProviderFailureKind::Authentication
        );
    }
}
