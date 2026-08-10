//! Interactive local operator console for infer-runtime.
//!
//! This module owns the console session lifecycle, its periodically refreshed
//! control-plane projection, and user actions. Rendering and child-process
//! supervision live in adjacent owners.

mod state;
mod telemetry;
mod ui;

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use infer_core::RuntimeConfig;
use tokio::process::Command;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    task::JoinHandle,
    time::MissedTickBehavior,
};

use self::state::{ConsoleState, Tab};
use crate::{
    daemon_supervisor::DaemonSupervisor,
    operator_client::{ConsoleSnapshot, OperatorClient},
};

const MIN_REFRESH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub(crate) struct ConsoleOptions {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) config: PathBuf,
    pub(crate) daemon_bin: Option<PathBuf>,
    pub(crate) spawn: bool,
    pub(crate) refresh_interval: Duration,
    pub(crate) max_logs: usize,
}

pub(crate) async fn run(options: ConsoleOptions) -> anyhow::Result<()> {
    let refresh_interval = options.refresh_interval.max(MIN_REFRESH_INTERVAL);
    let client = OperatorClient::new(options.base_url, options.api_key)?;
    let (mut supervisor, mut log_receiver) =
        DaemonSupervisor::new(options.daemon_bin, options.config.clone());
    let mut state = ConsoleState::new(options.config, options.max_logs);
    if options.spawn {
        if client.health_reachable().await {
            state.set_status(
                "an external inferd is already reachable; attached without spawning",
                false,
            );
        } else if state.config_valid {
            match supervisor.start().await {
                Ok(()) => state.set_status("inferd start requested", false),
                Err(error) => state.set_status(error.to_string(), true),
            }
        } else {
            state.set_status(
                "configuration is invalid; --spawn did not start inferd",
                true,
            );
        }
    }
    let (refresh_requests, mut snapshots, snapshot_task) =
        spawn_snapshot_poller(client.clone(), refresh_interval);

    let mut terminal = ratatui::init();
    let session_result = async {
        let mut running = true;
        while running {
            if let Some(status) = supervisor.poll_exit()? {
                state.set_status(format!("inferd exited with {status}"), !status.success());
                request_refresh(&refresh_requests);
            }
            state.drain_logs(&mut log_receiver);
            while let Ok(snapshot) = snapshots.try_recv() {
                state.accept_snapshot(snapshot);
            }

            terminal.draw(|frame| ui::render(frame, &state, &supervisor))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                running = handle_key(
                    key,
                    &mut state,
                    &mut supervisor,
                    &mut terminal,
                    &client,
                    &refresh_requests,
                )
                .await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    ratatui::restore();
    snapshot_task.abort();
    let stop_result = if supervisor.owns_running_process() {
        supervisor.stop().await
    } else {
        Ok(())
    };
    session_result?;
    stop_result
}

async fn handle_key(
    key: KeyEvent,
    state: &mut ConsoleState,
    supervisor: &mut DaemonSupervisor,
    terminal: &mut ratatui::DefaultTerminal,
    client: &OperatorClient,
    refresh_requests: &Sender<()>,
) -> anyhow::Result<bool> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(false);
    }
    if state.logs.editing_query {
        match key.code {
            KeyCode::Esc => state.logs.cancel_query(),
            KeyCode::Enter => state.logs.finish_query(),
            KeyCode::Backspace => state.logs.pop_query_char(),
            KeyCode::Char(value) => state.logs.push_query_char(value),
            _ => {}
        }
        return Ok(true);
    }
    match key.code {
        KeyCode::Char('q') => return Ok(false),
        KeyCode::Tab | KeyCode::Right => state.tab = state.tab.next(),
        KeyCode::BackTab | KeyCode::Left => state.tab = state.tab.previous(),
        KeyCode::Char(value @ '1'..='6') => {
            state.tab = Tab::from_index(value as usize - '1' as usize)
        }
        KeyCode::Char('s') => {
            state.validate_config();
            if !state.config_valid {
                state.set_status(
                    "configuration is invalid; validate before starting inferd",
                    true,
                );
            } else if state.snapshot.health.value.is_some() && !supervisor.owns_running_process() {
                state.set_status(
                    "an external inferd is already reachable; console will not start a competing daemon",
                    true,
                );
            } else {
                match supervisor.start().await {
                    Ok(()) => state.set_status("inferd start requested", false),
                    Err(error) => state.set_status(error.to_string(), true),
                }
                request_refresh(refresh_requests);
            }
        }
        KeyCode::Char('x') => {
            if supervisor.owns_running_process() {
                match supervisor.stop().await {
                    Ok(()) => state.set_status("console-owned inferd stopped", false),
                    Err(error) => state.set_status(error.to_string(), true),
                }
                request_refresh(refresh_requests);
            } else {
                state.set_status("there is no console-owned inferd process to stop", true);
            }
        }
        KeyCode::Char('r') => {
            state.validate_config();
            if !state.config_valid {
                state.set_status(
                    "configuration is invalid; validate before restarting inferd",
                    true,
                );
            } else if supervisor.owns_running_process() {
                match supervisor.restart().await {
                    Ok(()) => state.set_status("inferd restarted", false),
                    Err(error) => state.set_status(error.to_string(), true),
                }
                request_refresh(refresh_requests);
            } else {
                state.set_status("restart is limited to a console-owned inferd process", true);
            }
        }
        KeyCode::Char('v') => state.validate_config(),
        KeyCode::Char('e') => {
            ratatui::restore();
            let edit_result = open_editor(&state.config_path).await;
            *terminal = ratatui::init();
            match edit_result {
                Ok(()) => state.validate_config(),
                Err(error) => state.set_status(error.to_string(), true),
            }
        }
        KeyCode::Char('g') => request_refresh(refresh_requests),
        KeyCode::Up if state.tab == Tab::Jobs => state.move_job_selection(-1),
        KeyCode::Down if state.tab == Tab::Jobs => state.move_job_selection(1),
        KeyCode::Enter if state.tab == Tab::Jobs => {
            if let Some(job) = state.selected_job_target() {
                match client.explain_job(&job.id).await {
                    Ok(detail) => {
                        state.job_detail = Some(detail);
                        state.set_status(format!("loaded explain record for {}", job.id), false);
                    }
                    Err(error) => state.set_status(error, true),
                }
            } else {
                state.set_status("there is no selected Job to inspect", true);
            }
        }
        KeyCode::Char('C') if state.tab == Tab::Jobs => {
            if let Some(job) = state.selected_job_target() {
                if matches!(
                    job.state.as_str(),
                    "succeeded" | "failed" | "cancelled" | "expired"
                ) {
                    state.set_status(format!("job {} is already terminal", job.id), true);
                } else {
                    match client.cancel_job(&job.id).await {
                        Ok(_) => state.set_status(format!("cancelled job {}", job.id), false),
                        Err(error) => state.set_status(error, true),
                    }
                    request_refresh(refresh_requests);
                }
            } else {
                state.set_status("there is no selected Job to cancel", true);
            }
        }
        KeyCode::Up if state.tab == Tab::Resources => state.move_resource_selection(-1),
        KeyCode::Down if state.tab == Tab::Resources => state.move_resource_selection(1),
        KeyCode::Char('R') if state.tab == Tab::Resources => {
            match client.refresh_resources().await {
                Ok(_) => state.set_status("native resources refreshed", false),
                Err(error) => state.set_status(error, true),
            }
            request_refresh(refresh_requests);
        }
        KeyCode::Char('L') if state.tab == Tab::Resources => {
            resource_action(state, client, true).await;
            request_refresh(refresh_requests);
        }
        KeyCode::Char('U') if state.tab == Tab::Resources => {
            resource_action(state, client, false).await;
            request_refresh(refresh_requests);
        }
        KeyCode::Char('P') if state.tab == Tab::Resources => {
            if let Some(resource) = state.selected_resource_target() {
                match client.probe_provider(&resource.provider).await {
                    Ok(_) => state.set_status(
                        format!(
                            "provider {} compatibility probe completed",
                            resource.provider
                        ),
                        false,
                    ),
                    Err(error) => state.set_status(error, true),
                }
                request_refresh(refresh_requests);
            } else {
                state.set_status("there is no selected provider to probe", true);
            }
        }
        KeyCode::Char('z') if state.tab == Tab::Statistics => {
            state.statistics.clear();
            state.set_status("session statistics history cleared", false);
        }
        KeyCode::Char('/') if state.tab == Tab::Logs => state.logs.begin_query(),
        KeyCode::Char('f') if state.tab == Tab::Logs => state.logs.cycle_filter(),
        KeyCode::Char('p') if state.tab == Tab::Logs => state.logs.toggle_follow(),
        KeyCode::Char('c') if state.tab == Tab::Logs => {
            state.logs.clear();
            state.set_status("console log buffer cleared", false);
        }
        KeyCode::Up if state.tab == Tab::Logs => state.logs.scroll_up(1),
        KeyCode::Down if state.tab == Tab::Logs => state.logs.scroll_down(1),
        KeyCode::PageUp if state.tab == Tab::Logs => state.logs.scroll_up(10),
        KeyCode::PageDown if state.tab == Tab::Logs => state.logs.scroll_down(10),
        KeyCode::Home if state.tab == Tab::Logs => state.logs.jump_to_start(),
        KeyCode::End if state.tab == Tab::Logs => state.logs.jump_to_end(),
        _ => {}
    }
    Ok(true)
}

async fn resource_action(state: &mut ConsoleState, client: &OperatorClient, load: bool) {
    let Some(resource) = state.selected_resource_target() else {
        state.set_status("there is no selected managed deployment", true);
        return;
    };
    let result = if load {
        client
            .load_resource(&resource.provider, &resource.deployment)
            .await
    } else {
        client
            .unload_resource(&resource.provider, &resource.deployment)
            .await
    };
    match result {
        Ok(_) => state.set_status(
            format!(
                "{} deployment {}",
                if load { "loaded" } else { "unloaded" },
                resource.deployment
            ),
            false,
        ),
        Err(error) => state.set_status(error, true),
    }
}

fn spawn_snapshot_poller(
    client: OperatorClient,
    refresh_interval: Duration,
) -> (Sender<()>, Receiver<ConsoleSnapshot>, JoinHandle<()>) {
    let (refresh_requests, mut requested_refreshes): (Sender<()>, Receiver<()>) = mpsc::channel(1);
    let (snapshots, receiver) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut generation = 0_u64;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                request = requested_refreshes.recv() => {
                    if request.is_none() {
                        return;
                    }
                }
            }
            generation = generation.saturating_add(1);
            let snapshot = client.snapshot(generation).await;
            if snapshots.send(snapshot).await.is_err() {
                return;
            }
        }
    });
    (refresh_requests, receiver, task)
}

fn request_refresh(refresh_requests: &Sender<()>) {
    let _ = refresh_requests.try_send(());
}

async fn open_editor(path: &Path) -> anyhow::Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let executable = parts.next().context("VISUAL/EDITOR is empty")?;
    let status = Command::new(executable)
        .args(parts)
        .arg(path)
        .status()
        .await
        .with_context(|| format!("open {} with {editor}", path.display()))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("editor exited with {status}")
    }
}

fn validate_config(path: &Path) -> (bool, String) {
    match RuntimeConfig::load(path) {
        Ok(_) => (true, format!("configuration is valid: {}", path.display())),
        Err(error) => (false, format!("configuration is invalid: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Tab, validate_config};

    #[test]
    fn tab_navigation_is_stable_and_wraps() {
        assert_eq!(Tab::Overview.previous(), Tab::Config);
        assert_eq!(Tab::Config.next(), Tab::Overview);
        assert_eq!(Tab::from_index(2), Tab::Jobs);
    }

    #[test]
    fn checked_in_console_configuration_is_valid() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/infer.example.toml");
        let (valid, message) = validate_config(&path);
        assert!(valid, "{message}");
    }
}
