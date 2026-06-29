//! HTTP PUT uploader for remote HLS ingest targets.
//!
//! YouTube-style endpoints pass the target object name as a `file=` query
//! parameter. Other HLS PUT origins commonly use a playlist path and expect
//! segments beside it. This module supports both shapes.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::error;

use reqwest::{Client, Url};

use crate::media::engine::{EgressRegistration, MediaEngine};
use crate::media::hls::HlsStore;

const HLS_PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";
const HLS_SEGMENT_CONTENT_TYPE: &str = "video/mp2t";
const UPLOAD_INTERVAL: Duration = Duration::from_millis(500);
const HLS_UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn start_hls_put_upload(
    output_id: String,
    pipeline_id: String,
    target_url: String,
    store: Arc<HlsStore>,
    engine: Arc<MediaEngine>,
    registration: EgressRegistration,
) {
    engine
        .update_egress_phase_if_current(&output_id, &registration, "uploading")
        .await;
    let playlist_url = match Url::parse(&target_url) {
        Ok(url) => url,
        Err(err) => {
            error!(output_id = %output_id, err = %err, "invalid HLS upload URL");
            engine
                .record_egress_error_if_current(
                    &output_id,
                    &registration,
                    "parse_url",
                    err.to_string(),
                )
                .await;
            return;
        }
    };
    if let Some(host) = playlist_url.host_str() {
        let port = playlist_url
            .port_or_known_default()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        engine
            .update_egress_target_addr_if_current(
                &output_id,
                &registration,
                format!("{host}:{port}"),
            )
            .await;
    }
    let client = Client::new();
    let mut uploaded_segments = HashSet::new();

    loop {
        tokio::select! {
            _ = registration.cancel_token.cancelled() => return,
            _ = tokio::time::sleep(UPLOAD_INTERVAL) => {}
        }

        let Some(snapshot) = store.snapshot() else {
            continue;
        };

        for segment in snapshot.segments {
            if uploaded_segments.contains(&segment.index) {
                continue;
            }
            let segment_name = format!("seg{}.ts", segment.index);
            let segment_url = derive_hls_upload_url(&playlist_url, &segment_name);
            let segment_len = segment.data.len() as u64;
            match put_bytes_with_timeout(
                &client,
                segment_url,
                HLS_SEGMENT_CONTENT_TYPE,
                segment.data,
                HLS_UPLOAD_REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(()) => {
                    uploaded_segments.insert(segment.index);
                    engine
                        .record_egress_progress_if_current(&output_id, &registration, segment_len)
                        .await;
                }
                Err(err) => {
                    error!(
                        "[hls-upload] Segment upload failed output={} pipeline={} segment={}: {}",
                        output_id, pipeline_id, segment_name, err
                    );
                    engine
                        .record_egress_error_if_current(
                            &output_id,
                            &registration,
                            "upload_segment",
                            err,
                        )
                        .await;
                    return;
                }
            }
        }

        let playlist_bytes = snapshot.playlist.into_bytes();
        let playlist_len = playlist_bytes.len() as u64;
        if let Err(err) = put_bytes(
            &client,
            playlist_url.clone(),
            HLS_PLAYLIST_CONTENT_TYPE,
            playlist_bytes,
        )
        .await
        {
            error!(
                "[hls-upload] Playlist upload failed output={} pipeline={}: {}",
                output_id, pipeline_id, err
            );
            engine
                .record_egress_error_if_current(&output_id, &registration, "upload_playlist", err)
                .await;
            return;
        } else {
            engine
                .record_egress_progress_if_current(&output_id, &registration, playlist_len)
                .await;
        }
    }
}

async fn put_bytes<B>(
    client: &Client,
    url: Url,
    content_type: &'static str,
    body: B,
) -> Result<(), String>
where
    B: Into<reqwest::Body>,
{
    put_bytes_with_timeout(client, url, content_type, body, HLS_UPLOAD_REQUEST_TIMEOUT).await
}

async fn put_bytes_with_timeout<B>(
    client: &Client,
    url: Url,
    content_type: &'static str,
    body: B,
    timeout: Duration,
) -> Result<(), String>
where
    B: Into<reqwest::Body>,
{
    let status = client
        .put(url.clone())
        .timeout(timeout)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                format!("PUT {url} timed out after {} ms", timeout.as_millis())
            } else {
                err.to_string()
            }
        })?
        .status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("PUT {} returned {}", url, status))
    }
}

pub(crate) fn derive_hls_upload_url(playlist_url: &Url, file_name: &str) -> Url {
    let mut url = playlist_url.clone();
    let original_pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();

    if original_pairs.iter().any(|(key, _)| key == "file") {
        {
            let mut pairs = url.query_pairs_mut();
            pairs.clear();
            for (key, value) in original_pairs {
                if key == "file" {
                    pairs.append_pair(&key, file_name);
                } else {
                    pairs.append_pair(&key, &value);
                }
            }
        }
        return url;
    }

    let path = url.path();
    let new_path = if path.ends_with('/') {
        format!("{path}{file_name}")
    } else if path
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains('.'))
    {
        let prefix = path
            .rsplit_once('/')
            .map(|(prefix, _)| prefix)
            .unwrap_or("");
        if prefix.is_empty() {
            format!("/{file_name}")
        } else {
            format!("{prefix}/{file_name}")
        }
    } else {
        format!("{}/{}", path.trim_end_matches('/'), file_name)
    };
    url.set_path(&new_path);
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::OriginalUri;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::put;
    use std::sync::Mutex;

    #[test]
    fn derives_segment_url_from_file_query() {
        let playlist =
            Url::parse("https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=out.m3u8")
                .unwrap();
        let segment = derive_hls_upload_url(&playlist, "seg42.ts");
        assert_eq!(
            segment.as_str(),
            "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=seg42.ts"
        );
    }

    #[test]
    fn derives_segment_url_from_playlist_path() {
        let playlist = Url::parse("https://example.com/live/out.m3u8").unwrap();
        let segment = derive_hls_upload_url(&playlist, "seg42.ts");
        assert_eq!(segment.as_str(), "https://example.com/live/seg42.ts");
    }

    #[test]
    fn derives_segment_url_from_directory_path() {
        let playlist = Url::parse("https://example.com/live/channel/").unwrap();
        let segment = derive_hls_upload_url(&playlist, "seg42.ts");
        assert_eq!(
            segment.as_str(),
            "https://example.com/live/channel/seg42.ts"
        );
    }

    #[test]
    fn preserves_signed_query_for_path_style_uploads() {
        let playlist =
            Url::parse("https://example.com/live/out.m3u8?hdnea=token&policy=abc").unwrap();
        let segment = derive_hls_upload_url(&playlist, "seg42.ts");
        assert_eq!(
            segment.as_str(),
            "https://example.com/live/seg42.ts?hdnea=token&policy=abc"
        );
    }

    #[tokio::test]
    async fn uploads_segments_and_playlist_to_put_sink() {
        let seen = Arc::new(Mutex::new(Vec::<(String, String, Vec<u8>)>::new()));
        let seen_for_handler = seen.clone();
        let app = Router::new().route(
            "/*path",
            put(move |uri: OriginalUri, headers: HeaderMap, body: Bytes| {
                let seen = seen_for_handler.clone();
                async move {
                    let content_type = headers
                        .get(reqwest::header::CONTENT_TYPE.as_str())
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen.lock()
                        .unwrap()
                        .push((uri.0.to_string(), content_type, body.to_vec()));
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let store = Arc::new(HlsStore::new());
        store.push_segment(1.2, bytes::Bytes::from_static(b"segment-0"));
        let engine = Arc::new(MediaEngine::new());
        let registration = engine
            .register_egress_attempt(
                "out1",
                "pipe1",
                &format!("http://{addr}/upload?cid=abc&file=out.m3u8"),
            )
            .await;
        let uploader = tokio::spawn(start_hls_put_upload(
            "out1".to_string(),
            "pipe1".to_string(),
            format!("http://{addr}/upload?cid=abc&file=out.m3u8"),
            store,
            engine,
            registration.clone(),
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if seen.lock().unwrap().len() >= 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for PUT uploads"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        registration.cancel_token.cancel();
        let _ = uploader.await;

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().any(|(uri, content_type, body)| {
                uri == "/upload?cid=abc&file=seg0.ts"
                    && content_type == HLS_SEGMENT_CONTENT_TYPE
                    && body == b"segment-0"
            }),
            "segment PUT not observed: {seen:?}"
        );
        assert!(
            seen.iter().any(|(uri, content_type, body)| {
                uri == "/upload?cid=abc&file=out.m3u8"
                    && content_type == HLS_PLAYLIST_CONTENT_TYPE
                    && body.starts_with(b"#EXTM3U")
            }),
            "playlist PUT not observed: {seen:?}"
        );
    }

    #[tokio::test]
    async fn put_bytes_times_out_against_hung_sink() {
        let app = Router::new().route(
            "/*path",
            put(|| async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                StatusCode::NO_CONTENT
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = Client::new();
        let result = put_bytes_with_timeout(
            &client,
            Url::parse(&format!("http://{addr}/upload?file=out.m3u8")).unwrap(),
            HLS_PLAYLIST_CONTENT_TYPE,
            Bytes::from_static(b"#EXTM3U"),
            Duration::from_millis(50),
        )
        .await;

        let err = result.expect_err("hung sink should time out");
        assert!(
            err.to_ascii_lowercase().contains("timed out"),
            "expected timeout error, got: {err}"
        );
    }

    #[tokio::test]
    async fn uploader_exits_after_upload_error() {
        let app = Router::new().route("/*path", put(|| async { StatusCode::BAD_GATEWAY }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let store = Arc::new(HlsStore::new());
        store.push_segment(1.2, bytes::Bytes::from_static(b"segment-0"));
        let engine = Arc::new(MediaEngine::new());
        let registration = engine
            .register_egress_attempt(
                "out1",
                "pipe1",
                &format!("http://{addr}/upload?cid=abc&file=out.m3u8"),
            )
            .await;

        tokio::time::timeout(
            Duration::from_secs(2),
            start_hls_put_upload(
                "out1".to_string(),
                "pipe1".to_string(),
                format!("http://{addr}/upload?cid=abc&file=out.m3u8"),
                store,
                engine,
                registration,
            ),
        )
        .await
        .expect("uploader should exit promptly after an upload error");
    }
}
