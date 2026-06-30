use crate::application::ports::{MetaLookupError, MetaStore, MetaStoreWriter};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupFuture, MetaWriteFuture};
    use std::sync::Mutex;

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
}
