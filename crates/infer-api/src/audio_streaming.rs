//! WebSocket transport for typed duplex transcription sessions.

use axum::{
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::StreamExt;
use infer_control::{RuntimeError, RuntimeTranscriptionSession};
use infer_core::TranscriptionSessionRequest;
use infer_provider::ProviderFailureKind;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiError, ApiState, authenticate};

const MAX_AUDIO_FRAME_BYTES: usize = 1024 * 1024;
const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;

pub(super) async fn open_transcription_stream(
    State(state): State<ApiState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let app_id = authenticate(&state, &headers)?;
    Ok(upgrade
        .max_frame_size(MAX_AUDIO_FRAME_BYTES)
        .max_message_size(MAX_AUDIO_FRAME_BYTES)
        .on_upgrade(move |socket| run_session(state, app_id, socket)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigureMessage {
    #[serde(rename = "type")]
    kind: String,
    session: TranscriptionSessionRequest,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlMessage {
    #[serde(rename = "type")]
    kind: String,
}

async fn run_session(state: ApiState, app_id: String, mut socket: WebSocket) {
    let Some(first) = socket.next().await else {
        return;
    };
    let configure = match first {
        Ok(Message::Text(text)) if text.len() <= MAX_CONTROL_MESSAGE_BYTES => {
            serde_json::from_str::<ConfigureMessage>(&text).map_err(|error| error.to_string())
        }
        Ok(_) => Err("first frame must be a bounded session.configure JSON message".into()),
        Err(error) => Err(error.to_string()),
    };
    let configure = match configure {
        Ok(configure) if configure.kind == "session.configure" => configure,
        Ok(_) => {
            send_error(
                &mut socket,
                "invalid_session_message",
                "expected session.configure",
            )
            .await;
            return;
        }
        Err(message) => {
            send_error(&mut socket, "invalid_session_message", &message).await;
            return;
        }
    };
    let mut session = match state
        .runtime
        .open_transcription_session(&app_id, configure.session)
        .await
    {
        Ok(session) => session,
        Err(error) => {
            let (code, message) = public_runtime_error(&error);
            send_error(&mut socket, code, &message).await;
            return;
        }
    };
    let session_id = session.job_id().to_owned();
    if send_json(
        &mut socket,
        json!({
            "type": "session.created",
            "session": {
                "id": session_id.clone(),
                "execution_mode": "duplex",
                "stream_semantics": "revisable",
                "transcription_mode": "commit_redecode"
            }
        }),
    )
    .await
    .is_err()
    {
        let _ = session.cancel().await;
        return;
    }

    while let Some(frame) = socket.next().await {
        let action = match frame {
            Ok(Message::Binary(bytes)) => session.push_audio(bytes).await.map(|_| None),
            Ok(Message::Text(text)) if text.len() <= MAX_CONTROL_MESSAGE_BYTES => {
                handle_control(&mut session, &text).await.map(Some)
            }
            Ok(Message::Ping(payload)) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    let _ = session.cancel().await;
                    return;
                }
                continue;
            }
            Ok(Message::Pong(_)) => continue,
            Ok(Message::Close(_)) | Err(_) => {
                let _ = session.cancel().await;
                return;
            }
            Ok(_) => Err(RuntimeError::Contract(
                infer_core::ContractError::InvalidAudio("unsupported WebSocket frame".into()),
            )),
        };
        match action {
            Ok(Some(SessionAction::Revision(revision))) => {
                let is_final = revision.is_final;
                let event_type = if is_final {
                    "transcript.final"
                } else {
                    "transcript.partial"
                };
                if send_json(
                    &mut socket,
                    json!({"type": event_type, "transcript": revision}),
                )
                .await
                .is_err()
                {
                    let _ = session.cancel().await;
                    return;
                }
                if is_final {
                    let _ = send_json(
                        &mut socket,
                        json!({"type":"session.completed", "session_id": session_id}),
                    )
                    .await;
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: 1000,
                            reason: "completed".into(),
                        })))
                        .await;
                    return;
                }
            }
            Ok(Some(SessionAction::Cancelled)) => {
                let _ = session.cancel().await;
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: 1000,
                        reason: "cancelled".into(),
                    })))
                    .await;
                return;
            }
            Ok(None) => {}
            Err(error) => {
                let (code, message) = public_runtime_error(&error);
                send_error(&mut socket, code, &message).await;
                let _ = session.cancel().await;
                return;
            }
        }
    }
    let _ = session.cancel().await;
}

enum SessionAction {
    Revision(infer_core::TranscriptRevision),
    Cancelled,
}

async fn handle_control(
    session: &mut RuntimeTranscriptionSession,
    text: &str,
) -> Result<SessionAction, RuntimeError> {
    let message: ControlMessage = serde_json::from_str(text).map_err(|error| {
        RuntimeError::Contract(infer_core::ContractError::InvalidAudio(format!(
            "invalid session control message: {error}"
        )))
    })?;
    match message.kind.as_str() {
        "input_audio.commit" => session.commit(false).await.map(SessionAction::Revision),
        "input_audio.finish" => session.commit(true).await.map(SessionAction::Revision),
        "session.cancel" => Ok(SessionAction::Cancelled),
        _ => Err(RuntimeError::Contract(
            infer_core::ContractError::InvalidAudio("unknown session control message".into()),
        )),
    }
}

async fn send_json(socket: &mut WebSocket, value: Value) -> Result<(), ()> {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_| ())
}

async fn send_error(socket: &mut WebSocket, code: &str, message: &str) {
    let _ = send_json(
        socket,
        json!({"type":"error", "error":{"code":code, "message":message}}),
    )
    .await;
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 1008,
            reason: code.to_owned().into(),
        })))
        .await;
}

fn public_runtime_error(error: &RuntimeError) -> (&'static str, String) {
    match error {
        RuntimeError::Contract(_) => ("invalid_request_error", error.to_string()),
        RuntimeError::Unauthorized => ("invalid_api_key", "invalid API credential".into()),
        RuntimeError::UnknownApp(_)
        | RuntimeError::PolicyNotAllowed(_)
        | RuntimeError::OverrideNotAllowed { .. } => ("policy_violation", error.to_string()),
        RuntimeError::IntentNotAllowed { .. } => ("intent_forbidden", error.to_string()),
        RuntimeError::NoCandidate => ("no_candidate", error.to_string()),
        RuntimeError::QueueFull => ("queue_full", error.to_string()),
        RuntimeError::AppQueueFull => ("app_queue_full", error.to_string()),
        RuntimeError::QuotaExceeded { .. } => ("quota_exceeded", error.to_string()),
        RuntimeError::DeadlineExpired => ("deadline_exceeded", error.to_string()),
        RuntimeError::Cancelled => ("cancelled", error.to_string()),
        RuntimeError::ProviderUnavailable(_) => ("provider_unavailable", error.to_string()),
        RuntimeError::Provider(provider) => (
            match provider.kind() {
                ProviderFailureKind::Authentication => "upstream_authentication",
                ProviderFailureKind::RateLimited => "upstream_rate_limited",
                ProviderFailureKind::Timeout => "upstream_timeout",
                ProviderFailureKind::Unavailable => "upstream_unavailable",
                ProviderFailureKind::InvalidRequest => "upstream_invalid_request",
                ProviderFailureKind::Protocol => "upstream_protocol",
            },
            "audio provider failed".into(),
        ),
        _ => ("runtime_error", error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use infer_provider::ProviderError;

    use super::*;

    #[test]
    fn duplex_errors_reuse_the_http_machine_codes() {
        assert_eq!(
            public_runtime_error(&RuntimeError::IntentNotAllowed {
                app_id: "consumer".into(),
                intent: "audio.transcribe".into(),
            })
            .0,
            "intent_forbidden"
        );
        assert_eq!(
            public_runtime_error(&RuntimeError::QueueFull).0,
            "queue_full"
        );
        assert_eq!(
            public_runtime_error(&RuntimeError::Provider(ProviderError::Classified {
                kind: ProviderFailureKind::RateLimited,
                message: "quota".into(),
            }))
            .0,
            "upstream_rate_limited"
        );
    }
}
