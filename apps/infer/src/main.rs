mod console;
mod daemon_supervisor;
mod operator_client;
mod web_console;

use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

use futures_util::StreamExt;
use infer_artifact::ArtifactStore;
use infer_auth::AppCredentials;
use infer_core::{FivePointLandmarks, RuntimeConfig};
use reqwest::multipart::{Form, Part};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "infer", about = "Client and admin CLI for infer-runtime")]
struct Args {
    #[arg(long, env = "INFER_URL", default_value = "http://127.0.0.1:8787")]
    server: String,
    /// Explicit bearer credential. By default the selected App's
    /// runtime-managed credential is loaded from the configured local store.
    #[arg(long, env = "INFER_API_KEY")]
    api_key: Option<String>,
    #[arg(
        long,
        env = "INFER_APP_ID",
        default_value = "local-operator",
        global = true
    )]
    app_id: String,
    #[arg(
        long,
        env = "INFER_CONFIG",
        default_value = "config/infer.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify and atomically publish one configured ONNX Build into the shared
    /// content-addressed artifact store. This is local and does not call inferd.
    ImportOnnx {
        build_id: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Verify and publish one configured ONNX Build dependency, such as its
    /// tokenizer, into the same content-addressed artifact store.
    ImportOnnxAuxiliary {
        build_id: String,
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Open the browser-based local operator dashboard. It can attach to an
    /// existing daemon or own an inferd child for this Console session.
    Console {
        /// Explicit inferd binary. By default, use a sibling binary and then PATH.
        #[arg(long)]
        daemon_bin: Option<PathBuf>,
        /// Start inferd as a Console-owned child when the dashboard opens.
        #[arg(long)]
        spawn: bool,
        #[arg(long, default_value_t = 500)]
        max_logs: usize,
        /// Loopback address for the Web Console. Public binds are rejected.
        #[arg(long, default_value = "127.0.0.1:8790")]
        bind: SocketAddr,
        /// Print the Console URL without opening the default browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Open the legacy terminal dashboard as a low-dependency fallback.
    TerminalConsole {
        #[arg(long)]
        daemon_bin: Option<PathBuf>,
        #[arg(long)]
        spawn: bool,
        #[arg(long, default_value_t = 1000)]
        refresh_ms: u64,
        #[arg(long, default_value_t = 500)]
        max_logs: usize,
    },
    Run {
        #[arg(long, default_value = "language.respond")]
        route: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        stream: bool,
        #[arg(long)]
        background: bool,
    },
    Transcribe {
        #[arg(long, default_value = "audio.transcribe")]
        route: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        language: Option<String>,
    },
    /// Call the experimental typed face-detection protocol.
    DetectFaces {
        #[arg(long, default_value = "vision.detect_faces")]
        route: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        source_revision: String,
    },
    /// Call the experimental typed SFace embedding protocol.
    EmbedFace {
        #[arg(long, default_value = "vision.embed_face")]
        route: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        source_revision: String,
        /// Named five-point landmarks as strict JSON in input pixel coordinates.
        #[arg(long, value_name = "JSON")]
        landmarks: String,
    },
    Align {
        #[arg(long, default_value = "audio.align")]
        route: String,
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        text: String,
        #[arg(long)]
        language: Option<String>,
    },
    Speak {
        #[arg(long, default_value = "speech.synthesize")]
        route: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        voice: Option<String>,
        #[arg(long)]
        instructions: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long, default_value = "wav")]
        format: String,
        #[arg(long)]
        output: PathBuf,
    },
    CloneVoice {
        #[arg(long, default_value = "speech.clone_voice")]
        route: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        reference_audio: PathBuf,
        #[arg(long)]
        reference_text: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long, default_value = "wav")]
        format: String,
        #[arg(long)]
        output: PathBuf,
    },
    Status,
    Job {
        response_id: String,
    },
    /// Page through payload-free Job metadata for the authenticated App.
    Jobs {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        state: Option<String>,
    },
    /// Retrieve an encrypted durable Responses result or current status.
    Response {
        response_id: String,
    },
    /// Cancel a durable Responses background request.
    CancelResponse {
        response_id: String,
    },
    Cancel {
        response_id: String,
    },
    Explain {
        response_id: String,
    },
    /// Show configured quota limits, settled ledger entries, and outstanding
    /// reservations without accessing the daemon's SQLite file directly.
    Budget,
    /// Alias for the accounting section of `budget`; retained as the more
    /// natural operator verb for ledger inspection.
    Usage,
    /// Show active and pending work per provider queue.
    Queue,
    /// Show safe provider inventory, declared capabilities, and circuit state.
    Providers,
    /// Show the last observed native local-model inventory state.
    Resources,
    /// Explicitly refresh native local-model inventory state. This never loads
    /// or unloads a model.
    RefreshResources,
    /// Show durable, payload-free local resource action audit events.
    ResourceEvents {
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Recompute the eviction recommendation and unload exactly one model if
    /// its first target still matches the operator-approved deployment.
    ApplyEviction {
        deployment_id: String,
        #[arg(long)]
        reason: String,
    },
    /// Show background eviction monitor and active maintenance lease state.
    EvictionWindow,
    /// Grant a short process-local window in which the enabled pressure
    /// monitor may apply one target per poll.
    GrantEvictionWindow {
        #[arg(long, default_value_t = 900)]
        duration_seconds: u64,
        #[arg(long)]
        reason: String,
    },
    /// Revoke an active background eviction maintenance lease.
    RevokeEvictionWindow {
        lease_id: String,
    },
    /// Explicitly keep one configured Ollama deployment resident. This is an
    /// operator action and is never triggered by request routing.
    LoadModel {
        provider_id: String,
        deployment_id: String,
    },
    /// Drain and release one configured Ollama deployment from memory.
    UnloadModel {
        provider_id: String,
        deployment_id: String,
    },
    /// Measure native reload time for a currently non-resident model. The
    /// result includes a TOML fragment but never edits runtime config.
    BenchmarkModel {
        provider_id: String,
        deployment_id: String,
        #[arg(long, default_value_t = 3)]
        samples: u8,
        #[arg(long)]
        evidence: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if matches!(
        &args.command,
        Command::ImportOnnx { .. } | Command::ImportOnnxAuxiliary { .. }
    ) {
        let config = RuntimeConfig::load(&args.config).map_err(anyhow::Error::msg)?;
        let (build_id, file) = match &args.command {
            Command::ImportOnnx { build_id, file }
            | Command::ImportOnnxAuxiliary { build_id, file, .. } => (build_id, file),
            _ => unreachable!(),
        };
        let build = config
            .model_builds
            .get(build_id)
            .ok_or_else(|| anyhow::anyhow!("unknown model Build {build_id}"))?;
        let onnx = build
            .onnx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("model Build {build_id} is not ONNX"))?;
        let store = ArtifactStore::from_config(&config.artifacts)?;
        let (path, sha256, size_bytes, artifact_name) = match &args.command {
            Command::ImportOnnx { .. } => (
                store.publish_onnx(build_id, file, onnx)?,
                onnx.artifact.sha256.clone(),
                onnx.artifact.size_bytes,
                None,
            ),
            Command::ImportOnnxAuxiliary { name, .. } => {
                let identity = onnx.auxiliary_artifacts.get(name).ok_or_else(|| {
                    anyhow::anyhow!("ONNX Build {build_id} has no auxiliary artifact {name}")
                })?;
                (
                    store.publish_onnx_auxiliary(build_id, name, file, onnx)?,
                    identity.sha256.clone(),
                    identity.size_bytes,
                    Some(name),
                )
            }
            _ => unreachable!(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "build": build_id,
                "artifact": artifact_name,
                "sha256": sha256,
                "size_bytes": size_bytes,
                "path": path,
                "verified": true,
            }))?
        );
        return Ok(());
    }

    let api_key = resolve_api_key(&args)?;
    let client = consumer_http_client_builder().build()?;
    let base = args.server.trim_end_matches('/');
    match args.command {
        Command::ImportOnnx { .. } | Command::ImportOnnxAuxiliary { .. } => {
            unreachable!("handled before consumer authentication")
        }
        Command::Console {
            daemon_bin,
            spawn,
            max_logs,
            bind,
            no_open,
        } => {
            web_console::run(web_console::WebConsoleOptions {
                runtime_url: base.to_owned(),
                api_key: api_key.clone(),
                config: args.config.clone(),
                daemon_bin,
                spawn,
                bind,
                open_browser: !no_open,
                max_logs,
            })
            .await?;
        }
        Command::TerminalConsole {
            daemon_bin,
            spawn,
            refresh_ms,
            max_logs,
        } => {
            console::run(console::ConsoleOptions {
                base_url: base.to_owned(),
                api_key: api_key.clone(),
                config: args.config.clone(),
                daemon_bin,
                spawn,
                refresh_interval: Duration::from_millis(refresh_ms),
                max_logs,
            })
            .await?;
        }
        Command::Run {
            route,
            input,
            stream,
            background,
        } => {
            let response = client
                .post(format!("{base}/v1/responses"))
                .header(
                    infer_core::CAPABILITY_CONTRACT_HEADER,
                    "infer.responses@20260812.1",
                )
                .bearer_auth(&api_key)
                .json(&json!({
                    "model":route,
                    "input":input,
                    "stream":stream,
                    "background":background,
                }))
                .send()
                .await?
                .error_for_status()?;
            if stream {
                let mut bytes = response.bytes_stream();
                while let Some(chunk) = bytes.next().await {
                    print!("{}", String::from_utf8_lossy(&chunk?));
                }
            } else {
                println!("{}", response.text().await?);
            }
        }
        Command::Transcribe {
            route,
            file,
            language,
        } => {
            let mut form = audio_form(route, &file, "file").await?;
            if let Some(language) = language {
                form = form.text("language", language);
            }
            println!(
                "{}",
                client
                    .post(format!("{base}/v1/audio/transcriptions"))
                    .header(
                        infer_core::CAPABILITY_CONTRACT_HEADER,
                        "infer.audio.transcription@20260811.1",
                    )
                    .bearer_auth(&api_key)
                    .multipart(form)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?
            );
        }
        Command::DetectFaces {
            route,
            file,
            source_revision,
        } => {
            let form = vision_form(route, &file, source_revision).await?;
            println!(
                "{}",
                client
                    .post(format!("{base}/infer/v1/vision/face-detections"))
                    .header(
                        infer_core::CAPABILITY_CONTRACT_HEADER,
                        "infer.vision.face-detection@20260811.1",
                    )
                    .bearer_auth(&api_key)
                    .multipart(form)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?
            );
        }
        Command::EmbedFace {
            route,
            file,
            source_revision,
            landmarks,
        } => {
            let landmarks: FivePointLandmarks = serde_json::from_str(&landmarks)
                .map_err(|error| anyhow::anyhow!("invalid --landmarks JSON: {error}"))?;
            let form = vision_form(route, &file, source_revision)
                .await?
                .text("landmarks", serde_json::to_string(&landmarks)?);
            println!(
                "{}",
                client
                    .post(format!("{base}/infer/v1/vision/face-embeddings"))
                    .header(
                        infer_core::CAPABILITY_CONTRACT_HEADER,
                        "infer.vision.face-embedding@20260811.1",
                    )
                    .bearer_auth(&api_key)
                    .multipart(form)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?
            );
        }
        Command::Align {
            route,
            file,
            text,
            language,
        } => {
            let mut form = audio_form(route, &file, "file").await?.text("text", text);
            if let Some(language) = language {
                form = form.text("language", language);
            }
            println!(
                "{}",
                client
                    .post(format!("{base}/v1/audio/alignments"))
                    .header(
                        infer_core::CAPABILITY_CONTRACT_HEADER,
                        "infer.audio.alignment@20260811.1",
                    )
                    .bearer_auth(&api_key)
                    .multipart(form)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?
            );
        }
        Command::Speak {
            route,
            input,
            voice,
            instructions,
            language,
            format,
            output,
        } => {
            let response = client
                .post(format!("{base}/v1/audio/speech"))
                .header(
                    infer_core::CAPABILITY_CONTRACT_HEADER,
                    "infer.audio.speech@20260811.1",
                )
                .bearer_auth(&api_key)
                .json(&json!({
                    "model": route,
                    "input": input,
                    "voice": voice,
                    "instructions": instructions,
                    "language": language,
                    "response_format": format,
                }))
                .send()
                .await?
                .error_for_status()?;
            save_audio_response(response, &output).await?;
        }
        Command::CloneVoice {
            route,
            input,
            reference_audio,
            reference_text,
            language,
            format,
            output,
        } => {
            let mut form = audio_form(route, &reference_audio, "reference_audio")
                .await?
                .text("input", input)
                .text("reference_text", reference_text)
                .text("response_format", format);
            if let Some(language) = language {
                form = form.text("language", language);
            }
            let response = client
                .post(format!("{base}/v1/audio/voice-clones"))
                .header(
                    infer_core::CAPABILITY_CONTRACT_HEADER,
                    "infer.audio.voice-clone@20260811.1",
                )
                .bearer_auth(&api_key)
                .multipart(form)
                .send()
                .await?
                .error_for_status()?;
            save_audio_response(response, &output).await?;
        }
        Command::Status => println!(
            "{}",
            client
                .get(format!("{base}/health"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::Job { response_id } => println!(
            "{}",
            authed(
                &client,
                &api_key,
                format!("{base}/infer/v1/jobs/{response_id}")
            )
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
        ),
        Command::Jobs {
            limit,
            cursor,
            priority,
            state,
        } => {
            let mut query = vec![("limit", limit.to_string())];
            if let Some(cursor) = cursor {
                query.push(("cursor", cursor));
            }
            if let Some(priority) = priority {
                query.push(("priority", priority));
            }
            if let Some(state) = state {
                query.push(("state", state));
            }
            println!(
                "{}",
                authed(&client, &api_key, format!("{base}/infer/v1/jobs"))
                    .query(&query)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?
            );
        }
        Command::Response { response_id } => println!(
            "{}",
            capability_request(
                authed(
                &client,
                &api_key,
                format!("{base}/v1/responses/{response_id}")
                ),
                "infer.responses@20260812.1",
            )
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
        ),
        Command::CancelResponse { response_id } => println!(
            "{}",
            client
                .post(format!("{base}/v1/responses/{response_id}/cancel"))
                .header(
                    infer_core::CAPABILITY_CONTRACT_HEADER,
                    "infer.responses@20260812.1",
                )
                .bearer_auth(&api_key)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::Cancel { response_id } => println!(
            "{}",
            client
                .post(format!("{base}/infer/v1/jobs/{response_id}/cancel"))
                .bearer_auth(&api_key)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::Explain { response_id } => println!(
            "{}",
            authed(
                &client,
                &api_key,
                format!("{base}/infer/v1/explain/{response_id}")
            )
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
        ),
        Command::Budget | Command::Usage => println!(
            "{}",
            authed(&client, &api_key, format!("{base}/infer/v1/budget"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::Queue => println!(
            "{}",
            authed(&client, &api_key, format!("{base}/infer/v1/metrics"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::Providers => println!(
            "{}",
            authed(&client, &api_key, format!("{base}/infer/v1/providers"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::Resources => println!(
            "{}",
            authed(&client, &api_key, format!("{base}/infer/v1/resources"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::RefreshResources => println!(
            "{}",
            client
                .post(format!("{base}/infer/v1/resources"))
                .bearer_auth(&api_key)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::ResourceEvents { limit } => println!(
            "{}",
            authed(
                &client,
                &api_key,
                format!("{base}/infer/v1/resources/events?limit={limit}")
            )
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
        ),
        Command::ApplyEviction {
            deployment_id,
            reason,
        } => println!(
            "{}",
            client
                .post(format!("{base}/infer/v1/resources/eviction/apply"))
                .bearer_auth(&api_key)
                .json(&json!({
                    "expected_deployment": deployment_id,
                    "reason": reason,
                }))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::EvictionWindow => println!(
            "{}",
            authed(
                &client,
                &api_key,
                format!("{base}/infer/v1/resources/eviction/maintenance-lease")
            )
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
        ),
        Command::GrantEvictionWindow {
            duration_seconds,
            reason,
        } => println!(
            "{}",
            client
                .post(format!(
                    "{base}/infer/v1/resources/eviction/maintenance-lease"
                ))
                .bearer_auth(&api_key)
                .json(&json!({
                    "duration_ms": duration_seconds.saturating_mul(1000),
                    "reason": reason,
                }))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::RevokeEvictionWindow { lease_id } => println!(
            "{}",
            client
                .post(format!(
                    "{base}/infer/v1/resources/eviction/maintenance-lease/revoke"
                ))
                .bearer_auth(&api_key)
                .json(&json!({"lease_id": lease_id}))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::LoadModel {
            provider_id,
            deployment_id,
        } => println!(
            "{}",
            client
                .post(format!(
                    "{base}/infer/v1/resources/{provider_id}/deployments/{deployment_id}/load"
                ))
                .bearer_auth(&api_key)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::UnloadModel {
            provider_id,
            deployment_id,
        } => println!(
            "{}",
            client
                .post(format!(
                    "{base}/infer/v1/resources/{provider_id}/deployments/{deployment_id}/unload"
                ))
                .bearer_auth(&api_key)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
        Command::BenchmarkModel {
            provider_id,
            deployment_id,
            samples,
            evidence,
        } => println!(
            "{}",
            client
                .post(format!(
                    "{base}/infer/v1/resources/{provider_id}/deployments/{deployment_id}/benchmark-reload"
                ))
                .bearer_auth(&api_key)
                .json(&json!({"samples":samples,"evidence":evidence}))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
        ),
    }
    Ok(())
}

fn resolve_api_key(args: &Args) -> anyhow::Result<String> {
    if let Some(api_key) = &args.api_key {
        if api_key.trim().is_empty() {
            anyhow::bail!("--api-key/INFER_API_KEY must not be empty");
        }
        return Ok(api_key.clone());
    }
    let config = RuntimeConfig::load(&args.config)
        .map_err(anyhow::Error::msg)
        .map_err(|error| {
            error.context(format!(
                "load {} to resolve App credential",
                args.config.display()
            ))
        })?;
    let credentials = AppCredentials::load_or_create(&config)?;
    Ok(credentials.token_for(&args.app_id)?.to_owned())
}

pub(crate) fn consumer_http_client_builder() -> reqwest::ClientBuilder {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_bytes(infer_core::CONSUMER_CORE_HEADER.as_bytes())
            .expect("frozen Consumer contract header name is valid"),
        reqwest::header::HeaderValue::from_static(infer_core::CONSUMER_CORE_CONTRACT),
    );
    reqwest::Client::builder().default_headers(headers)
}

fn authed(client: &reqwest::Client, api_key: &str, url: String) -> reqwest::RequestBuilder {
    client.get(url).bearer_auth(api_key)
}

fn capability_request(
    request: reqwest::RequestBuilder,
    capability: &'static str,
) -> reqwest::RequestBuilder {
    request.header(infer_core::CAPABILITY_CONTRACT_HEADER, capability)
}

async fn audio_form(route: String, path: &PathBuf, field: &'static str) -> anyhow::Result<Form> {
    let bytes = tokio::fs::read(path).await?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("audio.wav")
        .to_owned();
    Ok(Form::new()
        .text("model", route)
        .part(field, Part::bytes(bytes).file_name(filename)))
}

async fn vision_form(
    route: String,
    path: &PathBuf,
    source_revision: String,
) -> anyhow::Result<Form> {
    let bytes = tokio::fs::read(path).await?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image.jpg")
        .to_owned();
    let content_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => anyhow::bail!("typed vision calls accept JPEG or PNG images"),
    };
    let image = Part::bytes(bytes)
        .file_name(filename)
        .mime_str(content_type)?;
    Ok(Form::new()
        .text("model", route)
        .text("source_revision", source_revision)
        .part("image", image))
}

async fn save_audio_response(response: reqwest::Response, output: &PathBuf) -> anyhow::Result<()> {
    let job_id = response
        .headers()
        .get("x-infer-job-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    tokio::fs::write(output, response.bytes().await?).await?;
    println!("saved {} (job {})", output.display(), job_id);
    Ok(())
}
