//! Application-layer persistence for transcode profiles.
//! This file owns the meta-table key, JSON round-tripping, and syncing the
//! persisted profile set into the runtime cache used by the media layer.

use crate::application::ports::{MetaLookupError, MetaStore, MetaStoreWriter};
use crate::domain::transcode_profile::TranscodeProfiles;
use tracing::{info, warn};

pub const TRANSCODE_PROFILES_META_KEY: &str = "transcode_profiles";

pub async fn load_transcode_profiles(meta_store: &dyn MetaStore) {
    let profiles = match meta_store.get_meta(TRANSCODE_PROFILES_META_KEY).await {
        Ok(Some(json_str)) => match serde_json::from_str::<TranscodeProfiles>(&json_str) {
            Ok(p) if !p.is_empty() => {
                info!(count = p.len(), "loaded profiles from meta store");
                p
            }
            Ok(_) => {
                warn!("meta store has empty profiles, using defaults");
                crate::media::profiles::built_in_defaults()
            }
            Err(error) => {
                warn!(err = %error, "failed to parse profiles, using defaults");
                crate::media::profiles::built_in_defaults()
            }
        },
        Ok(None) => {
            info!("no persisted profiles found, using built-in defaults");
            crate::media::profiles::built_in_defaults()
        }
        Err(error) => {
            warn!(err = %error, "failed to load profiles, using defaults");
            crate::media::profiles::built_in_defaults()
        }
    };

    crate::media::profiles::replace_runtime_profiles(&profiles).await;
}

pub async fn save_transcode_profiles(
    meta_store: &dyn MetaStoreWriter,
    profiles: &TranscodeProfiles,
) -> Result<(), MetaLookupError> {
    let json =
        serde_json::to_string(profiles).map_err(|error| MetaLookupError::new(error.to_string()))?;
    meta_store
        .set_meta(TRANSCODE_PROFILES_META_KEY, &json)
        .await?;
    crate::media::profiles::replace_runtime_profiles(profiles).await;
    info!(
        count = profiles.len(),
        "updated profiles in meta store and runtime cache"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::{MetaLookupFuture, MetaWriteFuture};
    use crate::domain::transcode_profile::{TranscodeProfile, TranscodeProfiles};
    use std::sync::Mutex;

    struct FakeMetaStore {
        value: Mutex<Option<String>>,
        fail: bool,
    }

    impl MetaStore for FakeMetaStore {
        fn get_meta<'a>(&'a self, key: &'a str) -> MetaLookupFuture<'a> {
            Box::pin(async move {
                if key != TRANSCODE_PROFILES_META_KEY {
                    return Err(MetaLookupError::new("unexpected key"));
                }
                if self.fail {
                    return Err(MetaLookupError::new("db unavailable"));
                }
                Ok(self.value.lock().unwrap_or_else(|e| e.into_inner()).clone())
            })
        }
    }

    impl MetaStoreWriter for FakeMetaStore {
        fn set_meta<'a>(&'a self, key: &'a str, value: &'a str) -> MetaWriteFuture<'a> {
            Box::pin(async move {
                if key != TRANSCODE_PROFILES_META_KEY {
                    return Err(MetaLookupError::new("unexpected key"));
                }
                if self.fail {
                    return Err(MetaLookupError::new("db unavailable"));
                }
                *self.value.lock().unwrap_or_else(|e| e.into_inner()) = Some(value.to_string());
                Ok(value.to_string())
            })
        }
    }

    fn custom_profiles() -> TranscodeProfiles {
        TranscodeProfiles::from([(
            "4k60".to_string(),
            TranscodeProfile {
                preset: "ultrafast".to_string(),
                tune: "zerolatency".to_string(),
                crf: 23,
                gop: 60,
                bframes: 0,
                bitrate: 20_000_000,
                max_bitrate: 24_000_000,
                width: 3840,
                height: 2160,
            },
        )])
    }

    #[tokio::test]
    async fn save_transcode_profiles_updates_runtime_cache_with_built_ins() {
        let store = FakeMetaStore {
            value: Mutex::new(None),
            fail: false,
        };

        save_transcode_profiles(&store, &custom_profiles())
            .await
            .unwrap();

        let runtime = crate::media::profiles::current_effective().await;
        assert!(runtime.contains_key("h264"));
        assert!(runtime.contains_key("720p"));
        assert_eq!(runtime["4k60"].width, 3840);
    }

    #[tokio::test]
    async fn load_transcode_profiles_populates_runtime_cache_with_built_ins() {
        let raw = serde_json::to_string(&custom_profiles()).unwrap();
        let store = FakeMetaStore {
            value: Mutex::new(Some(raw)),
            fail: false,
        };

        load_transcode_profiles(&store).await;

        let runtime = crate::media::profiles::current_effective().await;
        assert!(runtime.contains_key("h264"));
        assert!(runtime.contains_key("1080p"));
        assert_eq!(runtime["4k60"].height, 2160);
    }
}
