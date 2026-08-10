//! Serialized, validated access to the operator-owned runtime configuration.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use infer_core::RuntimeConfig;
use serde_json::{Value, json};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(super) struct RuntimeConfigFile {
    path: Arc<PathBuf>,
}

impl RuntimeConfigFile {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path: Arc::new(path),
        }
    }

    pub(super) fn path(&self) -> &Path {
        self.path.as_ref().as_path()
    }

    pub(super) async fn read_source(&self) -> anyhow::Result<String> {
        fs::read_to_string(self.path())
            .await
            .with_context(|| format!("read runtime configuration {}", self.path().display()))
    }

    pub(super) fn load(&self) -> anyhow::Result<RuntimeConfig> {
        RuntimeConfig::load(self.path()).map_err(anyhow::Error::from)
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        self.load().map(|_| ())
    }

    pub(super) fn validation_message(&self) -> Value {
        match self.validate() {
            Ok(()) => json!({"valid": true, "message": "Configuration is valid"}),
            Err(error) => json!({"valid": false, "message": error.to_string()}),
        }
    }

    pub(super) async fn write_validated(&self, source: &str) -> anyhow::Result<()> {
        Self::validate_source(source)?;
        atomic_write(self.path(), source.as_bytes()).await
    }

    pub(super) fn validate_source(source: &str) -> anyhow::Result<RuntimeConfig> {
        let parsed =
            toml::from_str::<RuntimeConfig>(source).context("parse runtime configuration")?;
        parsed.validate().map_err(anyhow::Error::from)?;
        Ok(parsed)
    }
}

async fn atomic_write(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("infer.toml");
    let temporary = parent.join(format!(".{filename}.{}.tmp", Uuid::new_v4()));
    let write_result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        file.write_all(contents).await?;
        file.sync_all().await?;
        drop(file);
        fs::rename(&temporary, path).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    write_result.context("save validated runtime configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_source_never_replaces_valid_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let source = include_str!("../../../../config/infer.example.toml");
        let path = temp.path().join("infer.toml");
        std::fs::write(&path, source).unwrap();
        let file = RuntimeConfigFile::new(path);

        assert!(file.write_validated("unknown = true").await.is_err());
        assert_eq!(file.read_source().await.unwrap(), source);
    }
}
