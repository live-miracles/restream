use crate::application::ports::{MetaLookupError, MetaStore, MetaStoreWriter};
use crate::application::reconcile::RecordingCommand;
use crate::media::engine::MediaEngine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const RECORDING_SETTINGS_META_KEY: &str = "recording_settings";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RecordingSettings {
    pub retain_source_ts: bool,
}

pub fn recording_enabled_meta_key(pipeline_id: &str) -> String {
    format!("recording_enabled:{pipeline_id}")
}

pub async fn load_recording_enabled(meta_store: &dyn MetaStore, pipeline_id: &str) -> bool {
    meta_store
        .get_meta(&recording_enabled_meta_key(pipeline_id))
        .await
        .ok()
        .flatten()
        .is_some_and(|value| value == "1")
}

pub async fn load_recording_enabled_map(
    meta_store: &dyn MetaStore,
    pipeline_ids: &[String],
) -> HashMap<String, bool> {
    let mut enabled = HashMap::with_capacity(pipeline_ids.len());
    for pipeline_id in pipeline_ids {
        enabled.insert(
            pipeline_id.clone(),
            load_recording_enabled(meta_store, pipeline_id).await,
        );
    }
    enabled
}

pub async fn load_recording_settings(meta_store: &dyn MetaStore) -> RecordingSettings {
    meta_store
        .get_meta(RECORDING_SETTINGS_META_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<RecordingSettings>(&raw).ok())
        .unwrap_or_default()
}

pub async fn save_recording_settings(
    meta_store: &dyn MetaStoreWriter,
    settings: &RecordingSettings,
) -> Result<(), MetaLookupError> {
    let raw =
        serde_json::to_string(settings).map_err(|error| MetaLookupError::new(error.to_string()))?;
    meta_store
        .set_meta(RECORDING_SETTINGS_META_KEY, &raw)
        .await
        .map(|_| ())
}

pub async fn spawn_recording_task(
    engine: Arc<MediaEngine>,
    pipeline_name: String,
    pipeline_id: String,
    input_source: Option<String>,
    media_dir: String,
    recording_settings: RecordingSettings,
) -> CancellationToken {
    let ring_buffer = engine.get_or_create_pipeline(&pipeline_id).await;
    let cancel_token = engine.register_recording(&pipeline_id).await;
    let cancel_token_for_task = cancel_token.clone();
    let engine_for_task = engine.clone();
    let pipeline_id_for_cleanup = pipeline_id.clone();

    tokio::spawn(async move {
        crate::media::recording::start_recording(
            pipeline_name,
            pipeline_id.clone(),
            input_source,
            media_dir,
            recording_settings,
            ring_buffer,
            engine_for_task.clone(),
            cancel_token_for_task,
        )
        .await;
        engine_for_task
            .unregister_recording(&pipeline_id_for_cleanup)
            .await;
    });

    cancel_token
}

pub async fn apply_recording_commands(
    engine: Arc<MediaEngine>,
    meta_store: &dyn MetaStore,
    media_dir: &str,
    commands: Vec<RecordingCommand>,
) {
    let needs_settings = commands
        .iter()
        .any(|command| matches!(command, RecordingCommand::Start { .. }));
    let recording_settings = if needs_settings {
        Some(load_recording_settings(meta_store).await)
    } else {
        None
    };

    for command in commands {
        match command {
            RecordingCommand::Start {
                pipeline_name,
                pipeline_id,
                input_source,
            } => {
                spawn_recording_task(
                    engine.clone(),
                    pipeline_name,
                    pipeline_id,
                    input_source,
                    media_dir.to_string(),
                    recording_settings.clone().unwrap_or_default(),
                )
                .await;
            }
            RecordingCommand::Stop { pipeline_id } => {
                engine.unregister_recording(&pipeline_id).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupFuture, MetaWriteFuture};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    struct FakeMetaStore {
        values: Mutex<HashMap<String, String>>,
        fail_keys: HashMap<String, String>,
    }

    impl MetaStore for FakeMetaStore {
        fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.fail_keys.get(key) {
                    return Err(MetaLookupError::new(message.clone()));
                }
                Ok(self
                    .values
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(key)
                    .cloned())
            })
        }
    }

    impl MetaStoreWriter for FakeMetaStore {
        fn set_meta<'a>(&'a self, key: &'a str, value: &'a str) -> MetaWriteFuture<'a> {
            Box::pin(async move {
                self.values
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key.to_string(), value.to_string());
                Ok(value.to_string())
            })
        }
    }

    #[test]
    fn recording_enabled_meta_key_prefixes_pipeline_id() {
        assert_eq!(
            recording_enabled_meta_key("pipeline-a"),
            "recording_enabled:pipeline-a"
        );
    }

    #[tokio::test]
    async fn load_recording_enabled_reads_truthy_meta_value() {
        let store = FakeMetaStore {
            values: Mutex::new(HashMap::from([(
                "recording_enabled:pipeline-a".to_string(),
                "1".to_string(),
            )])),
            fail_keys: HashMap::new(),
        };

        assert!(load_recording_enabled(&store, "pipeline-a").await);
        assert!(!load_recording_enabled(&store, "pipeline-b").await);
    }

    #[tokio::test]
    async fn load_recording_enabled_map_treats_lookup_errors_as_disabled() {
        let store = FakeMetaStore {
            values: Mutex::new(HashMap::from([(
                "recording_enabled:pipeline-a".to_string(),
                "1".to_string(),
            )])),
            fail_keys: HashMap::from([(
                "recording_enabled:pipeline-b".to_string(),
                "db unavailable".to_string(),
            )]),
        };

        let enabled = load_recording_enabled_map(
            &store,
            &["pipeline-a".to_string(), "pipeline-b".to_string()],
        )
        .await;

        assert_eq!(enabled.get("pipeline-a"), Some(&true));
        assert_eq!(enabled.get("pipeline-b"), Some(&false));
    }

    #[tokio::test]
    async fn load_recording_settings_defaults_when_missing() {
        let store = FakeMetaStore {
            values: Mutex::new(HashMap::new()),
            fail_keys: HashMap::new(),
        };

        assert_eq!(
            load_recording_settings(&store).await,
            RecordingSettings {
                retain_source_ts: false,
            }
        );
    }

    #[tokio::test]
    async fn save_recording_settings_serializes_to_meta_store() {
        let store = FakeMetaStore {
            values: Mutex::new(HashMap::new()),
            fail_keys: HashMap::new(),
        };

        save_recording_settings(
            &store,
            &RecordingSettings {
                retain_source_ts: true,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            store
                .values
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(RECORDING_SETTINGS_META_KEY)
                .cloned(),
            Some("{\"retainSourceTs\":true}".to_string())
        );
    }

    #[tokio::test]
    async fn spawn_recording_task_registers_and_cleans_up_recording() {
        let engine = Arc::new(MediaEngine::new());
        let media_dir = unique_test_media_dir("recording-launch");

        let cancel_token = spawn_recording_task(
            engine.clone(),
            "Launch Test".to_string(),
            "pipeline-launch".to_string(),
            None,
            media_dir.display().to_string(),
            RecordingSettings::default(),
        )
        .await;

        assert!(engine.is_recording_active("pipeline-launch").await);

        cancel_token.cancel();
        wait_for_recording_shutdown(&engine, "pipeline-launch").await;
        assert!(!engine.is_recording_active("pipeline-launch").await);

        let _ = std::fs::remove_dir_all(media_dir);
    }

    #[tokio::test]
    async fn apply_recording_commands_starts_and_stops_recordings() {
        let engine = Arc::new(MediaEngine::new());
        let media_dir = unique_test_media_dir("recording-commands");
        let store = FakeMetaStore {
            values: Mutex::new(HashMap::from([(
                RECORDING_SETTINGS_META_KEY.to_string(),
                "{\"retainSourceTs\":true}".to_string(),
            )])),
            fail_keys: HashMap::new(),
        };
        let _existing = engine.register_recording("pipeline-stop").await;

        apply_recording_commands(
            engine.clone(),
            &store,
            media_dir.to_str().unwrap_or_default(),
            vec![
                RecordingCommand::Start {
                    pipeline_name: "Start Me".to_string(),
                    pipeline_id: "pipeline-start".to_string(),
                    input_source: None,
                },
                RecordingCommand::Stop {
                    pipeline_id: "pipeline-stop".to_string(),
                },
            ],
        )
        .await;

        assert!(engine.is_recording_active("pipeline-start").await);
        assert!(!engine.is_recording_active("pipeline-stop").await);

        engine.unregister_recording("pipeline-start").await;
        wait_for_recording_shutdown(&engine, "pipeline-start").await;
        let _ = std::fs::remove_dir_all(media_dir);
    }

    fn unique_test_media_dir(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&path).expect("test media dir should be created");
        path
    }

    async fn wait_for_recording_shutdown(engine: &Arc<MediaEngine>, pipeline_id: &str) {
        for _ in 0..50 {
            if !engine.is_recording_active(pipeline_id).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("recording task did not shut down in time");
    }
}
