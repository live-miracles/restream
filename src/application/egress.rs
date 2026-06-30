use crate::application::output_path::OutputPath;
use crate::media::engine::MediaEngine;
use crate::media::ring_buffer::RingBuffer;
use crate::types::Output;
use std::sync::Arc;

pub async fn prepare_output_ring(engine: &Arc<MediaEngine>, output: &Output) -> Arc<RingBuffer> {
    let source_buf = engine.get_or_create_pipeline(&output.pipeline_id).await;
    let output_path =
        OutputPath::resolve(output.pipeline_id.as_str(), &output.encoding, &output.url);
    let ingest_video_codec = engine.ingest_video_codec(&output.pipeline_id).await;
    let ingest_codec_override = output_path.ingest_codec_override(ingest_video_codec.as_deref());

    let video_buf = if let Some(stage) = output_path.video_stage() {
        engine
            .get_or_create_transcoder(
                &output.pipeline_id,
                stage.kind,
                source_buf.clone(),
                ingest_codec_override,
            )
            .await
    } else {
        source_buf.clone()
    };

    let pre_h264_buf = if let Some(stage) = output_path.audio_stage() {
        engine
            .get_or_create_transcoder(&output.pipeline_id, stage.kind, video_buf.clone(), None)
            .await
    } else {
        video_buf.clone()
    };

    if output_path.needs_rtmp_h264_conv(ingest_video_codec.as_deref()) {
        engine
            .get_or_create_h264_transcoder(
                &output.pipeline_id,
                output_path.codec_edge_upstream_kind().clone(),
                pre_h264_buf,
            )
            .await
    } else {
        pre_h264_buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::stage::StageKind;
    use crate::media::engine::VideoMeta;

    fn test_output(pipeline_id: &str, encoding: &str, url: &str) -> Output {
        Output {
            id: format!("{pipeline_id}-out"),
            pipeline_id: pipeline_id.to_string(),
            name: "Output".to_string(),
            url: url.to_string(),
            monitoring_url: None,
            desired_state: "running".to_string(),
            encoding: encoding.to_string(),
        }
    }

    #[tokio::test]
    async fn prepare_output_ring_reuses_source_ring_for_passthrough_output() {
        let engine = Arc::new(MediaEngine::new());
        let source = engine.get_or_create_pipeline("pipe-source").await;
        let output = test_output("pipe-source", "source", "srt://example:9000");

        let ring = prepare_output_ring(&engine, &output).await;

        assert!(Arc::ptr_eq(&source, &ring));
    }

    #[tokio::test]
    async fn prepare_output_ring_routes_hevc_rtmp_through_shared_h264_stage() {
        let engine = Arc::new(MediaEngine::new());
        engine
            .try_register_ingest("pipe-hevc", "stream-key", "rtmp")
            .await
            .unwrap();
        engine
            .update_ingest_meta(
                "pipe-hevc",
                Some(VideoMeta {
                    codec: "hevc".to_string(),
                    ..Default::default()
                }),
                None,
                None,
            )
            .await;
        let source = engine.get_or_create_pipeline("pipe-hevc").await;
        let expected = engine
            .get_or_create_h264_transcoder("pipe-hevc", StageKind::source(), source)
            .await;
        let output = test_output("pipe-hevc", "source", "rtmp://example/live/test");

        let ring = prepare_output_ring(&engine, &output).await;

        assert!(Arc::ptr_eq(&expected, &ring));
        assert_eq!(ring.codec_hint_str(), "h264");
    }
}
