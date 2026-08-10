//! Read-only host pressure observation lifecycle.
//!
//! This owner intentionally never refreshes provider-native inventory and
//! never applies eviction. It performs one startup sample before the daemon is
//! exposed, then keeps the pressure snapshot current at a low fixed cadence.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use infer_resource::ResourceManager;
use tokio::{
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval_at},
};

pub(crate) struct PressureObservation {
    task: Option<JoinHandle<()>>,
}

impl PressureObservation {
    pub(crate) async fn start(resources: Arc<ResourceManager>, interval: Duration) -> Self {
        resources.refresh_system_pressure().await;
        let task = tokio::spawn(run(Arc::downgrade(&resources), interval));
        Self { task: Some(task) }
    }

    #[cfg(test)]
    async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for PressureObservation {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run(resources: Weak<ResourceManager>, refresh_interval: Duration) {
    let first_refresh = Instant::now() + refresh_interval;
    let mut interval = interval_at(first_refresh, refresh_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Some(resources) = resources.upgrade() else {
            break;
        };
        resources.refresh_system_pressure().await;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use infer_core::RuntimeConfig;
    use infer_resource::{
        DynNativeModelController, InventoryState, NativeControlError, NativeControllerMap,
        NativeInventory, ResourceManager, SystemPressureLevel, SystemPressureSampler,
        SystemPressureSnapshot,
    };

    use super::PressureObservation;

    struct CountingPressureSampler {
        calls: AtomicUsize,
        snapshot: SystemPressureSnapshot,
    }

    impl CountingPressureSampler {
        fn normal() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                snapshot: SystemPressureSnapshot {
                    source: "test".into(),
                    level: SystemPressureLevel::Normal,
                    last_checked_unix_ms: 1,
                    total_memory_bytes: Some(1024),
                    free_memory_percent: Some(80),
                    last_error: None,
                },
            }
        }

        fn failed() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                snapshot: SystemPressureSnapshot {
                    source: "test".into(),
                    level: SystemPressureLevel::Unknown,
                    last_checked_unix_ms: 1,
                    total_memory_bytes: None,
                    free_memory_percent: None,
                    last_error: Some("sample failed".into()),
                },
            }
        }
    }

    impl SystemPressureSampler for CountingPressureSampler {
        fn sample(&self) -> SystemPressureSnapshot {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.snapshot.clone()
        }
    }

    struct CountingInventory {
        discovers: AtomicUsize,
    }

    #[async_trait]
    impl infer_resource::NativeModelController for CountingInventory {
        async fn discover(&self) -> Result<NativeInventory, NativeControlError> {
            self.discovers.fetch_add(1, Ordering::SeqCst);
            Ok(NativeInventory::default())
        }

        async fn load(&self, _model: &str) -> Result<(), NativeControlError> {
            Ok(())
        }

        async fn unload(&self, _model: &str) -> Result<(), NativeControlError> {
            Ok(())
        }
    }

    fn config() -> RuntimeConfig {
        toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap()
    }

    #[tokio::test]
    async fn startup_samples_pressure_without_querying_native_inventory() {
        let pressure = Arc::new(CountingPressureSampler::normal());
        let inventory = Arc::new(CountingInventory {
            discovers: AtomicUsize::new(0),
        });
        let mut controllers = NativeControllerMap::new();
        controllers.insert(
            "onnx-local".into(),
            Arc::clone(&inventory) as DynNativeModelController,
        );
        let resources = Arc::new(ResourceManager::with_pressure_sampler_and_controllers(
            &config(),
            Arc::clone(&pressure) as Arc<dyn SystemPressureSampler>,
            controllers,
        ));

        let observation =
            PressureObservation::start(Arc::clone(&resources), Duration::from_secs(60)).await;
        let snapshot = resources.snapshot().await;

        assert_eq!(pressure.calls.load(Ordering::SeqCst), 1);
        assert_eq!(inventory.discovers.load(Ordering::SeqCst), 0);
        assert_eq!(snapshot.system_pressure.level, SystemPressureLevel::Normal);
        assert!(
            snapshot
                .providers
                .iter()
                .all(|provider| provider.state == InventoryState::Unknown)
        );
        observation.shutdown().await;
    }

    #[tokio::test]
    async fn observation_refreshes_repeatedly_and_stops_with_its_owner() {
        let pressure = Arc::new(CountingPressureSampler::normal());
        let mut config = config();
        config.providers.clear();
        config.deployments.clear();
        let resources = Arc::new(ResourceManager::with_pressure_sampler(
            &config,
            Arc::clone(&pressure) as Arc<dyn SystemPressureSampler>,
        ));

        let observation =
            PressureObservation::start(Arc::clone(&resources), Duration::from_millis(10)).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while pressure.calls.load(Ordering::SeqCst) < 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("pressure observation should keep refreshing");

        observation.shutdown().await;
        let stopped_at = pressure.calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(pressure.calls.load(Ordering::SeqCst), stopped_at);
    }

    #[tokio::test]
    async fn failed_startup_sample_remains_diagnostic_unknown() {
        let pressure = Arc::new(CountingPressureSampler::failed());
        let mut config = config();
        config.providers.clear();
        config.deployments.clear();
        let resources = Arc::new(ResourceManager::with_pressure_sampler(
            &config,
            Arc::clone(&pressure) as Arc<dyn SystemPressureSampler>,
        ));

        let observation =
            PressureObservation::start(Arc::clone(&resources), Duration::from_secs(60)).await;
        let snapshot = resources.snapshot().await.system_pressure;

        assert_eq!(snapshot.level, SystemPressureLevel::Unknown);
        assert_eq!(snapshot.last_error.as_deref(), Some("sample failed"));
        assert!(!snapshot.is_pending());
        observation.shutdown().await;
    }
}
