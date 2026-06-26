#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <stdio.h>
#include <string.h>

static int name_list_contains(const char *list, const char *name) {
    const char *cursor = list;
    size_t want = strlen(name);

    if (list == NULL)
        return 0;

    while (*cursor != '\0') {
        const char *end = strchr(cursor, ',');
        size_t have = end != NULL ? (size_t)(end - cursor) : strlen(cursor);
        if (have == want && strncmp(cursor, name, want) == 0)
            return 1;
        if (end == NULL)
            break;
        cursor = end + 1;
    }

    return 0;
}

static int require_encoder(const char *name) {
    if (avcodec_find_encoder_by_name(name) != NULL)
        return 0;
    fprintf(stderr, "missing FFmpeg encoder: %s\n", name);
    return 1;
}

static int require_decoder(enum AVCodecID id, const char *name) {
    if (avcodec_find_decoder(id) != NULL)
        return 0;
    fprintf(stderr, "missing FFmpeg decoder: %s\n", name);
    return 1;
}

static int require_protocol(const char *name, int output) {
    void *opaque = NULL;
    const char *protocol = NULL;

    while ((protocol = avio_enum_protocols(&opaque, output)) != NULL) {
        if (strcmp(protocol, name) == 0)
            return 0;
    }

    fprintf(
        stderr,
        "missing FFmpeg %s protocol: %s\n",
        output ? "output" : "input",
        name
    );
    return 1;
}

static int require_demuxer(const char *name) {
    void *opaque = NULL;
    const AVInputFormat *fmt = NULL;

    while ((fmt = av_demuxer_iterate(&opaque)) != NULL) {
        if (name_list_contains(fmt->name, name))
            return 0;
    }

    fprintf(stderr, "missing FFmpeg demuxer: %s\n", name);
    return 1;
}

static int require_muxer(const char *name) {
    void *opaque = NULL;
    const AVOutputFormat *fmt = NULL;

    while ((fmt = av_muxer_iterate(&opaque)) != NULL) {
        if (name_list_contains(fmt->name, name))
            return 0;
    }

    fprintf(stderr, "missing FFmpeg muxer: %s\n", name);
    return 1;
}

int main(void) {
    int failed = 0;
    failed |= require_encoder("libx264");
    failed |= require_encoder("libx265");
    failed |= require_encoder("aac");
    failed |= require_encoder("ac3");
    failed |= require_decoder(AV_CODEC_ID_H264, "h264");
    failed |= require_decoder(AV_CODEC_ID_HEVC, "hevc");
    failed |= require_decoder(AV_CODEC_ID_AAC, "aac");
    failed |= require_decoder(AV_CODEC_ID_MP3, "mp3");
    failed |= require_decoder(AV_CODEC_ID_AC3, "ac3");
    failed |= require_decoder(AV_CODEC_ID_EAC3, "eac3");
    failed |= require_protocol("file", 0);
    failed |= require_protocol("pipe", 0);
    failed |= require_protocol("pipe", 1);
    failed |= require_demuxer("mpegts");
    failed |= require_demuxer("matroska");
    failed |= require_demuxer("mov");
    failed |= require_muxer("mpegts");
    failed |= require_muxer("matroska");

    if (failed)
        return 1;

    puts("Static FFmpeg codec/protocol/format capabilities: PASS");
    return 0;
}
