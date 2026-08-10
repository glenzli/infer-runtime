//! Lifecycle owner for an `inferd` process started by a local operator UI.

use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    sync::mpsc::{self, Receiver, Sender, error::TrySendError},
};

const LOG_CHANNEL_CAPACITY: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogSource {
    System,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl LogSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "console",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LogLine {
    pub(crate) recorded_at_unix_ms: u64,
    pub(crate) source: LogSource,
    pub(crate) level: LogLevel,
    pub(crate) text: String,
}

impl LogLine {
    fn new(source: LogSource, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            recorded_at_unix_ms: unix_ms(),
            level: classify_level(source, &text),
            source,
            text,
        }
    }

    pub(crate) fn console_audit(text: impl Into<String>) -> Self {
        Self::new(LogSource::System, text)
    }
}

pub(crate) struct DaemonSupervisor {
    daemon_bin: PathBuf,
    config: PathBuf,
    child: Option<Child>,
    started_at: Option<Instant>,
    logs: Sender<LogLine>,
    dropped_logs: Arc<AtomicU64>,
}

impl DaemonSupervisor {
    pub(crate) fn new(daemon_bin: Option<PathBuf>, config: PathBuf) -> (Self, Receiver<LogLine>) {
        let (logs, receiver) = mpsc::channel(LOG_CHANNEL_CAPACITY);
        let dropped_logs = Arc::new(AtomicU64::new(0));
        (
            Self {
                daemon_bin: daemon_bin.unwrap_or_else(resolve_daemon_binary),
                config,
                child: None,
                started_at: None,
                logs,
                dropped_logs,
            },
            receiver,
        )
    }

    pub(crate) async fn start(&mut self) -> anyhow::Result<()> {
        self.poll_exit()?;
        if self.child.is_some() {
            bail!("console already owns a running inferd process");
        }

        let mut command = Command::new(&self.daemon_bin);
        command
            .arg("--config")
            .arg(&self.config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().with_context(|| {
            format!(
                "start {} with {}",
                self.daemon_bin.display(),
                self.config.display()
            )
        })?;
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(
                stdout,
                LogSource::Stdout,
                self.logs.clone(),
                self.dropped_logs.clone(),
            );
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(
                stderr,
                LogSource::Stderr,
                self.logs.clone(),
                self.dropped_logs.clone(),
            );
        }
        self.started_at = Some(Instant::now());
        self.system_log(format!(
            "started inferd pid={} config={}",
            child.id().unwrap_or_default(),
            self.config.display()
        ));
        self.child = Some(child);
        Ok(())
    }

    pub(crate) async fn stop(&mut self) -> anyhow::Result<()> {
        let Some(mut child) = self.child.take() else {
            self.system_log("no console-owned inferd process to stop");
            return Ok(());
        };
        let status = match child.try_wait()? {
            Some(status) => status,
            None => {
                request_shutdown(&mut child).await?;
                match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                    Ok(status) => status.context("wait for inferd shutdown")?,
                    Err(_) => {
                        self.system_log("inferd did not stop within 5s; forcing process cleanup");
                        child.start_kill().context("force inferd shutdown")?;
                        child
                            .wait()
                            .await
                            .context("wait for forced inferd shutdown")?
                    }
                }
            }
        };
        self.started_at = None;
        self.system_log(format!("inferd stopped with {status}"));
        Ok(())
    }

    pub(crate) async fn restart(&mut self) -> anyhow::Result<()> {
        self.stop().await?;
        self.start().await
    }

    pub(crate) fn poll_exit(&mut self) -> anyhow::Result<Option<ExitStatus>> {
        let exited = match self.child.as_mut() {
            Some(child) => child.try_wait().context("observe inferd process")?,
            None => None,
        };
        if let Some(status) = exited {
            self.child = None;
            self.started_at = None;
            self.system_log(format!("inferd exited with {status}"));
            return Ok(Some(status));
        }
        Ok(None)
    }

    pub(crate) fn owns_running_process(&self) -> bool {
        self.child.is_some()
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub(crate) fn uptime_seconds(&self) -> Option<u64> {
        self.started_at.map(|started| started.elapsed().as_secs())
    }

    pub(crate) fn daemon_bin(&self) -> &Path {
        &self.daemon_bin
    }

    fn system_log(&self, message: impl Into<String>) {
        try_log(
            &self.logs,
            &self.dropped_logs,
            LogLine::new(LogSource::System, message),
        );
    }
}

#[cfg(unix)]
async fn request_shutdown(child: &mut Child) -> anyhow::Result<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    let result = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .await;
    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(_) | Err(_) => child.start_kill().context("request inferd shutdown"),
    }
}

#[cfg(not(unix))]
async fn request_shutdown(child: &mut Child) -> anyhow::Result<()> {
    child.start_kill().context("request inferd shutdown")
}

fn resolve_daemon_binary() -> PathBuf {
    let filename = if cfg!(windows) {
        "inferd.exe"
    } else {
        "inferd"
    };
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join(filename);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(filename)
}

fn spawn_reader<R>(
    reader: R,
    source: LogSource,
    logs: Sender<LogLine>,
    dropped_logs: Arc<AtomicU64>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(text)) => {
                    try_log(&logs, &dropped_logs, LogLine::new(source, text));
                }
                Ok(None) => return,
                Err(error) => {
                    try_log(
                        &logs,
                        &dropped_logs,
                        LogLine::new(
                            LogSource::System,
                            format!("failed to read {}: {error}", source.label()),
                        ),
                    );
                    return;
                }
            }
        }
    });
}

fn try_log(logs: &Sender<LogLine>, dropped_logs: &AtomicU64, line: LogLine) {
    let dropped = dropped_logs.swap(0, Ordering::Relaxed);
    if dropped != 0 {
        match logs.try_send(LogLine::new(
            LogSource::System,
            format!("dropped {dropped} daemon log line(s) while the Console was busy"),
        )) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                dropped_logs.fetch_add(dropped.saturating_add(1), Ordering::Relaxed);
                return;
            }
            Err(TrySendError::Closed(_)) => return,
        }
    }
    if matches!(logs.try_send(line), Err(TrySendError::Full(_))) {
        dropped_logs.fetch_add(1, Ordering::Relaxed);
    }
}

fn classify_level(source: LogSource, text: &str) -> LogLevel {
    let lowercase = text.to_ascii_lowercase();
    if lowercase.contains(" error")
        || lowercase.starts_with("error")
        || lowercase.contains("failed")
        || lowercase.contains("panic")
    {
        LogLevel::Error
    } else if lowercase.contains(" warn")
        || lowercase.starts_with("warn")
        || source == LogSource::Stderr
    {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::sync::mpsc;

    use super::{LogLevel, LogLine, LogSource, classify_level, resolve_daemon_binary, try_log};

    #[test]
    fn daemon_resolution_always_produces_a_nonempty_path() {
        assert!(!resolve_daemon_binary().as_os_str().is_empty());
    }

    #[test]
    fn log_level_projection_is_conservative_for_stderr() {
        assert_eq!(
            classify_level(LogSource::Stdout, "request ok"),
            LogLevel::Info
        );
        assert_eq!(
            classify_level(LogSource::Stdout, "WARN retry"),
            LogLevel::Warn
        );
        assert_eq!(
            classify_level(LogSource::Stderr, "worker output"),
            LogLevel::Warn
        );
        assert_eq!(
            classify_level(LogSource::Stderr, "request failed"),
            LogLevel::Error
        );
    }

    #[test]
    fn bounded_log_transport_reports_dropped_lines_when_capacity_returns() {
        let (logs, mut receiver) = mpsc::channel(2);
        let dropped = AtomicU64::new(0);
        logs.try_send(LogLine::new(LogSource::Stdout, "first"))
            .unwrap();
        logs.try_send(LogLine::new(LogSource::Stdout, "second"))
            .unwrap();
        try_log(&logs, &dropped, LogLine::new(LogSource::Stdout, "dropped"));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(receiver.try_recv().unwrap().text, "first");
        assert_eq!(receiver.try_recv().unwrap().text, "second");

        try_log(&logs, &dropped, LogLine::new(LogSource::Stdout, "after"));
        assert!(receiver.try_recv().unwrap().text.contains("dropped 1"));
        assert_eq!(receiver.try_recv().unwrap().text, "after");
    }
}
