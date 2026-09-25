//! One Agent task, one private workspace, and one dynamically scoped sandbox.

use std::{
    fs,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use infer_core::{AgentTaskInputFile, AgentTaskRequest};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{CodexAppServerProvider, CodexSession};
use crate::{AgentTaskExecution, ProviderError};

const PROFILE: &str = "infer_agent_task";
const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 64 * 1024;

impl CodexAppServerProvider {
    pub(super) async fn execute_bounded_agent_task(
        &self,
        request: AgentTaskRequest,
        model: &str,
    ) -> Result<AgentTaskExecution, ProviderError> {
        request
            .validate()
            .map_err(|error| ProviderError::InvalidInput(error.into()))?;
        if !self.admitted_models.contains(model) {
            return Err(ProviderError::InvalidInput(
                "Agent model is not admitted".into(),
            ));
        }

        let outer = tempfile::tempdir()?;
        let workspace = tempfile::Builder::new()
            .prefix("task-")
            .tempdir_in(outer.path())?;
        let home = outer.path().join("codex-home");
        fs::create_dir(&home)?;
        let input = workspace.path().join("input");
        let output = workspace.path().join("output");
        fs::create_dir(&input)?;
        fs::create_dir(&output)?;
        for file in &request.input_files {
            let path = input.join(&file.path);
            fs::create_dir_all(path.parent().expect("validated input path"))?;
            let bytes = STANDARD
                .decode(&file.content_base64)
                .map_err(|_| ProviderError::InvalidInput("invalid input base64".into()))?;
            fs::write(&path, &bytes)?;
            if format!("{:x}", Sha256::digest(fs::read(&path)?)) != file.sha256 {
                return Err(ProviderError::Protocol(
                    "staged Agent input digest changed".into(),
                ));
            }
        }
        for path in &request.output_paths {
            let target = output.join(path);
            fs::create_dir_all(target.parent().expect("validated output path"))?;
            fs::File::create_new(&target)?;
        }

        let source_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex")
            });
        let source_auth = fs::canonicalize(source_home.join("auth.json"))
            .map_err(|_| ProviderError::InvalidInput("Codex login is unavailable".into()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source_auth, home.join("auth.json"))?;
        #[cfg(not(unix))]
        return Err(ProviderError::InvalidInput(
            "Agent isolation is unavailable on this platform".into(),
        ));

        let binary = resolve_binary(&self.command)?;
        let entries = permission_entries(
            workspace.path(),
            &input,
            &output,
            &request.output_paths,
            &home,
            &source_home,
            &source_auth,
            &binary,
        );
        let profile_value = entries
            .iter()
            .map(|(path, access)| format!("{}={}", json!(path), json!(access)))
            .collect::<Vec<_>>()
            .join(",");
        let mut args = self.args.clone();
        args.extend([
            "--strict-config".into(),
            "-c".into(),
            format!("permissions.{PROFILE}.filesystem={{{profile_value}}}"),
            "-c".into(),
            format!("default_permissions=\"{PROFILE}\""),
            "-c".into(),
            "history.persistence=\"none\"".into(),
            "-c".into(),
            "web_search=\"disabled\"".into(),
        ]);
        for feature in [
            "plugins",
            "apps",
            "multi_agent",
            "computer_use",
            "browser_use",
            "browser_use_external",
        ] {
            args.extend(["--disable".into(), feature.into()]);
        }
        let binary_text = binary
            .to_str()
            .ok_or_else(|| ProviderError::InvalidInput("Codex path is not UTF-8".into()))?;
        let mut session =
            CodexSession::spawn_in(binary_text, &args, workspace, Some(&home)).await?;
        verify_profile(&mut session, &entries).await?;

        let cwd = session.workspace_path().display().to_string();
        let thread = session.request("thread/start", json!({
            "cwd": cwd, "model": model, "approvalPolicy": "never", "permissions": PROFILE,
            "ephemeral": true, "serviceName": "infer-runtime-agent-task",
            "baseInstructions": "Use only the submitted input/ files and declared output/ paths. Do not use network, apps, MCP, browser, or delegation. Never request broader permissions."
        })).await?;
        let thread_id = required_id(&thread, "/thread/id", "thread/start")?;
        let turn = session
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id, "approvalPolicy": "never", "permissions": PROFILE,
                    "input": [{"type":"text", "text": request.instruction}], "summary": "none"
                }),
            )
            .await
            .map_err(|_| ProviderError::RemoteOutcomeUnknown)?;
        let turn_id = required_id(&turn, "/turn/id", "turn/start")?;
        let answer = collect_turn(&mut session, &thread_id, &turn_id).await?;
        let outputs = collect_outputs(&output, &request.output_paths)?;
        Ok(AgentTaskExecution {
            answer,
            outputs,
            thread_id,
            turn_id,
            sandbox_profile: PROFILE.into(),
            tool_policy: "submitted-input-read;declared-output-write;network-off;approval-deny"
                .into(),
        })
    }
}

fn resolve_binary(command: &str) -> Result<PathBuf, ProviderError> {
    let path = Path::new(command);
    if path.is_absolute() {
        return Ok(fs::canonicalize(path)?);
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if let Ok(path) = fs::canonicalize(dir.join(command)) {
            return Ok(path);
        }
    }
    Err(ProviderError::InvalidInput(
        "Codex executable is unavailable".into(),
    ))
}

fn permission_entries(
    workspace: &Path,
    input: &Path,
    output: &Path,
    output_paths: &[String],
    home: &Path,
    source_home: &Path,
    source_auth: &Path,
    binary: &Path,
) -> Vec<(String, &'static str)> {
    let mut entries = vec![
        (":root".into(), "deny"),
        (":minimal".into(), "read"),
        (":tmpdir".into(), "deny"),
        (":slash_tmp".into(), "deny"),
        (workspace.display().to_string(), "read"),
        (input.display().to_string(), "read"),
        (output.display().to_string(), "read"),
        (home.display().to_string(), "deny"),
        (source_home.display().to_string(), "deny"),
        (source_auth.display().to_string(), "deny"),
        (binary.display().to_string(), "read"),
    ];
    entries.extend(
        output_paths
            .iter()
            .map(|path| (output.join(path).display().to_string(), "write")),
    );
    entries
}

async fn verify_profile(
    session: &mut CodexSession,
    entries: &[(String, &'static str)],
) -> Result<(), ProviderError> {
    let response = session.request("config/read", json!({})).await?;
    let config = response
        .get("config")
        .ok_or_else(|| ProviderError::Protocol("config/read omitted config".into()))?;
    if config
        .get("mcp_servers")
        .and_then(Value::as_object)
        .is_some_and(|items| !items.is_empty())
        || config
            .get("plugins")
            .and_then(Value::as_object)
            .is_some_and(|items| !items.is_empty())
        || config.get("default_permissions").and_then(Value::as_str) != Some(PROFILE)
    {
        return Err(ProviderError::Protocol(
            "Agent inherited tools or wrong permissions".into(),
        ));
    }
    let loaded = config
        .pointer(&format!("/permissions/{PROFILE}/filesystem"))
        .and_then(Value::as_object)
        .ok_or_else(|| ProviderError::Protocol("Agent permission profile was not loaded".into()))?;
    if entries
        .iter()
        .any(|(path, access)| loaded.get(path).and_then(Value::as_str) != Some(*access))
    {
        return Err(ProviderError::Protocol(
            "Agent task grant was not applied".into(),
        ));
    }
    Ok(())
}

fn required_id(value: &Value, pointer: &str, stage: &str) -> Result<String, ProviderError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::Protocol(format!("{stage} omitted identity")))
}

async fn collect_turn(
    session: &mut CodexSession,
    thread_id: &str,
    turn_id: &str,
) -> Result<String, ProviderError> {
    let mut answer = String::new();
    loop {
        let message = session
            .next_message()
            .await
            .map_err(|_| ProviderError::RemoteOutcomeUnknown)?;
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if message.get("id").is_some() && !method.is_empty() {
            let decision = approval_denial(method).ok_or(ProviderError::RemoteOutcomeUnknown)?;
            session
                .write(json!({"id": message["id"], "result": decision}))
                .await
                .map_err(|_| ProviderError::RemoteOutcomeUnknown)?;
            return Err(ProviderError::Protocol(
                "Agent requested permission outside task grant".into(),
            ));
        }
        let params = message.get("params").unwrap_or(&Value::Null);
        if params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|id| id != thread_id)
            || params
                .get("turnId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != turn_id)
        {
            continue;
        }
        match method {
            "item/started" | "item/completed" => {
                let item = params
                    .get("item")
                    .ok_or_else(|| ProviderError::Protocol("Agent item omitted payload".into()))?;
                let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
                if !matches!(
                    kind,
                    "userMessage"
                        | "reasoning"
                        | "agentMessage"
                        | "commandExecution"
                        | "fileChange"
                ) {
                    return Err(ProviderError::RemoteOutcomeUnknown);
                }
                if method == "item/completed" && kind == "agentMessage" {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if text.len() > MAX_ANSWER_BYTES {
                            return Err(ProviderError::Protocol(
                                "Agent answer exceeds limit".into(),
                            ));
                        }
                        answer = text.into();
                    }
                }
            }
            "turn/completed" => {
                if params.pointer("/turn/status").and_then(Value::as_str) != Some("completed") {
                    return Err(ProviderError::Protocol(
                        "Agent turn did not complete".into(),
                    ));
                }
                return Ok(answer);
            }
            _ => {}
        }
    }
}

fn approval_denial(method: &str) -> Option<Value> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Some(json!({"decision":"decline"}))
        }
        "item/permissions/requestApproval" => Some(json!({"permissions":{}})),
        _ => None,
    }
}

fn collect_outputs(
    root: &Path,
    declared: &[String],
) -> Result<Vec<AgentTaskInputFile>, ProviderError> {
    let mut outputs = Vec::with_capacity(declared.len());
    let mut total_bytes = 0_u64;
    for name in declared {
        let path = root.join(name);
        let mut current = path.as_path();
        while current != root {
            let metadata = fs::symlink_metadata(current)?;
            if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
                return Err(ProviderError::Protocol(
                    "Agent output is not a regular file".into(),
                ));
            }
            current = current
                .parent()
                .ok_or_else(|| ProviderError::Protocol("Agent output escaped root".into()))?;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > MAX_OUTPUT_BYTES {
            return Err(ProviderError::Protocol(
                "Agent output exceeds file limit".into(),
            ));
        }
        total_bytes += metadata.len();
        if total_bytes > 16 * 1024 * 1024 {
            return Err(ProviderError::Protocol(
                "Agent output exceeds task limit".into(),
            ));
        }
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
            return Err(ProviderError::Protocol(
                "Agent output has an external hardlink".into(),
            ));
        }
        let bytes = fs::read(&path)?;
        outputs.push(AgentTaskInputFile {
            path: name.clone(),
            content_base64: STANDARD.encode(&bytes),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        });
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn output_grant_names_only_declared_files() {
        let paths = vec!["a.txt".into(), "nested/b.txt".into()];
        let entries = permission_entries(
            Path::new("/task"),
            Path::new("/task/input"),
            Path::new("/task/output"),
            &paths,
            Path::new("/auth"),
            Path::new("/source-auth"),
            Path::new("/source-auth/auth.json"),
            Path::new("/codex"),
        );
        let writable = entries
            .iter()
            .filter(|(_, access)| *access == "write")
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            writable,
            ["/task/output/a.txt", "/task/output/nested/b.txt"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn server_approval_requests_receive_denials() {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}' ;;
    *'"method":"initialized"'*) printf '{"jsonrpc":"2.0","id":99,"method":"%s","params":{}}\n' "$1" ;;
    *'"id":99'*'"result"'*) printf '{"method":"test/observed","params":%s}\n' "$line"; exit 0 ;;
  esac
done
"#;
        for (method, expected) in [
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"decline"}),
            ),
            (
                "item/fileChange/requestApproval",
                json!({"decision":"decline"}),
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{}}),
            ),
        ] {
            let args = vec![
                "-c".into(),
                script.into(),
                "infer-test".into(),
                method.into(),
            ];
            let mut session =
                CodexSession::spawn_in("/bin/sh", &args, tempfile::tempdir().unwrap(), None)
                    .await
                    .unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                collect_turn(&mut session, "thread-test", "turn-test"),
            )
            .await
            .unwrap();
            assert!(result.is_err());
            let observed = tokio::time::timeout(std::time::Duration::from_secs(3), session.read())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(observed["method"], "test/observed");
            assert_eq!(observed["params"]["result"], expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn output_collector_rejects_links() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("secret.txt");
        fs::write(&target, "excluded").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("link.txt")).unwrap();
        assert!(collect_outputs(root.path(), &["link.txt".into()]).is_err());
        fs::hard_link(&target, root.path().join("hardlink.txt")).unwrap();
        assert!(collect_outputs(root.path(), &["hardlink.txt".into()]).is_err());
    }

    #[tokio::test]
    #[ignore = "requires a signed-in Codex subscription and performs a cloud Agent turn"]
    async fn live_bounded_agent_turn() {
        let outside = tempfile::tempdir().unwrap();
        let excluded = outside.path().join("excluded.txt");
        fs::write(&excluded, "FORBIDDEN-SYNTHETIC-SENTINEL").unwrap();
        let provider = CodexAppServerProvider::new(
            "codex-agent",
            "codex",
            vec!["app-server".into(), "--listen".into(), "stdio://".into()],
            BTreeSet::from(["gpt-6-sol".into()]),
        );
        let bytes = b"ALLOWED-SYNTHETIC-INPUT";
        let request = AgentTaskRequest {
            model: "agent.file_task".into(),
            instruction: format!(
                "Use a shell tool to read input/a.txt. Also attempt to read {}. Write output/result.txt with two lines: the exact input text and either EXCLUDED_DENIED or EXCLUDED_READ depending on the second read. Do not include the excluded file's content.",
                excluded.display()
            ),
            input_files: vec![AgentTaskInputFile {
                path: "a.txt".into(),
                content_base64: STANDARD.encode(bytes),
                sha256: format!("{:x}", Sha256::digest(bytes)),
            }],
            output_paths: vec!["result.txt".into()],
        };
        let execution = provider
            .execute_bounded_agent_task(request, "gpt-6-sol")
            .await
            .unwrap();
        let output = STANDARD
            .decode(&execution.outputs[0].content_base64)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("ALLOWED-SYNTHETIC-INPUT"), "{text}");
        assert!(text.contains("EXCLUDED_DENIED"), "{text}");
        assert!(!text.contains("FORBIDDEN-SYNTHETIC-SENTINEL"));
    }
}
