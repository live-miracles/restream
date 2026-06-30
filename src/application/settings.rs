//! Application-layer settings orchestration that assembles persisted config and
//! runtime-backed defaults into the snapshot exposed by the API layer.
//! This file owns cross-source settings reads; HTTP response shaping remains in
//! `crate::api`, while persistence details stay behind the existing ports.

use crate::application::ports::SqliteMetaStore;
use crate::application::recording::{RecordingSettings, load_recording_settings};
use crate::application::srt_ingest::load_global_srt_ingest_config;
use crate::domain::ingest_security::IngestSecurityConfig;
use crate::domain::srt_ingest::SrtGlobalIngestConfig;
use crate::domain::transcode_profile::TranscodeProfiles;
use crate::media::security::IngestSecurityService;
use sqlx::SqlitePool;

#[derive(Clone, Debug)]
pub struct SettingsSnapshot {
    pub server_name: String,
    pub ingest_host: String,
    pub ingest_security: IngestSecurityConfig,
    pub recording_settings: RecordingSettings,
    pub srt_ingest: SrtGlobalIngestConfig,
    pub transcode_profiles: TranscodeProfiles,
}

pub async fn load_settings_snapshot(
    pool: &SqlitePool,
    security: &IngestSecurityService,
) -> Result<SettingsSnapshot, sqlx::Error> {
    let server_name = crate::db::get_meta(pool, "server_name")
        .await?
        .unwrap_or_else(|| "Name".to_string());
    let ingest_host = crate::db::get_ingest_host(pool).await?.unwrap_or_default();
    let meta_store = SqliteMetaStore::new(pool.clone());
    let recording_settings = load_recording_settings(&meta_store).await;
    let srt_ingest = load_global_srt_ingest_config(&meta_store).await;
    let transcode_profiles = crate::media::profiles::current_effective().await;

    Ok(SettingsSnapshot {
        server_name,
        ingest_host,
        ingest_security: security.get_config(),
        recording_settings,
        srt_ingest,
        transcode_profiles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::domain::ingest_security::DEFAULT_INGEST_SECURITY_CONFIG;
    use crate::media::security::IngestSecurityService;

    #[tokio::test]
    async fn load_settings_snapshot_combines_db_meta_and_runtime_defaults() {
        let pool = db::create_pool("sqlite::memory:").await.unwrap();
        db::setup_database_schema(&pool).await.unwrap();
        db::set_meta(&pool, "server_name", "Restream Control")
            .await
            .unwrap();
        db::set_ingest_host(&pool, "ingest.example.com")
            .await
            .unwrap();

        let security = IngestSecurityService::new(DEFAULT_INGEST_SECURITY_CONFIG);
        let snapshot = load_settings_snapshot(&pool, &security).await.unwrap();

        assert_eq!(snapshot.server_name, "Restream Control");
        assert_eq!(snapshot.ingest_host, "ingest.example.com");
        assert_eq!(
            snapshot.ingest_security.failure_limit,
            DEFAULT_INGEST_SECURITY_CONFIG.failure_limit
        );
        assert_eq!(snapshot.recording_settings, RecordingSettings::default());
        assert_eq!(snapshot.srt_ingest, SrtGlobalIngestConfig::default());
        assert!(snapshot.transcode_profiles.contains_key("h264"));
    }
}
