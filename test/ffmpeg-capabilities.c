#include <libavcodec/avcodec.h>
#include <stdio.h>

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

int main(void) {
    int failed = 0;
    failed |= require_encoder("libx264");
    failed |= require_encoder("aac");
    failed |= require_encoder("ac3");
    failed |= require_decoder(AV_CODEC_ID_H264, "h264");
    failed |= require_decoder(AV_CODEC_ID_HEVC, "hevc");
    failed |= require_decoder(AV_CODEC_ID_AAC, "aac");
    failed |= require_decoder(AV_CODEC_ID_MP3, "mp3");
    failed |= require_decoder(AV_CODEC_ID_AC3, "ac3");
    failed |= require_decoder(AV_CODEC_ID_EAC3, "eac3");

    if (failed)
        return 1;

    puts("Static FFmpeg codec capabilities: PASS");
    return 0;
}
