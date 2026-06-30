use crate::application::ports::MetaStore;
use std::collections::HashMap;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupError, MetaLookupFuture, MetaStore};

    struct FakeMetaStore {
        values: HashMap<String, String>,
        fail_keys: HashMap<String, String>,
    }

    impl MetaStore for FakeMetaStore {
        fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a> {
            Box::pin(async move {
                if let Some(message) = self.fail_keys.get(key) {
                    return Err(MetaLookupError::new(message.clone()));
                }
                Ok(self.values.get(key).cloned())
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
            values: HashMap::from([("recording_enabled:pipeline-a".to_string(), "1".to_string())]),
            fail_keys: HashMap::new(),
        };

        assert!(load_recording_enabled(&store, "pipeline-a").await);
        assert!(!load_recording_enabled(&store, "pipeline-b").await);
    }

    #[tokio::test]
    async fn load_recording_enabled_map_treats_lookup_errors_as_disabled() {
        let store = FakeMetaStore {
            values: HashMap::from([("recording_enabled:pipeline-a".to_string(), "1".to_string())]),
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
}
