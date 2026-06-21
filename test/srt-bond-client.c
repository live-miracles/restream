#include <arpa/inet.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <srt/srt.h>

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s <broadcast|backup> <port>\n", argv[0]);
        return 2;
    }

    int group_type;
    if (strcmp(argv[1], "broadcast") == 0)
        group_type = SRT_GTYPE_BROADCAST;
    else if (strcmp(argv[1], "backup") == 0)
        group_type = SRT_GTYPE_BACKUP;
    else {
        fprintf(stderr, "unknown group type: %s\n", argv[1]);
        return 2;
    }

    srt_startup();
    SRTSOCKET group = srt_create_group(group_type);
    if (group == SRT_INVALID_SOCK) {
        fprintf(stderr, "group creation failed: %s\n", srt_getlasterror_str());
        return 1;
    }

    struct sockaddr_in address;
    memset(&address, 0, sizeof(address));
    address.sin_family = AF_INET;
    address.sin_port = htons((uint16_t)atoi(argv[2]));
    inet_pton(AF_INET, "127.0.0.1", &address.sin_addr);

    SRT_SOCKGROUPCONFIG members[2];
    members[0] = srt_prepare_endpoint(NULL, (struct sockaddr *)&address, sizeof(address));
    members[1] = srt_prepare_endpoint(NULL, (struct sockaddr *)&address, sizeof(address));
    if (group_type == SRT_GTYPE_BACKUP) {
        members[0].weight = 1;
        members[1].weight = 0;
    }

    if (srt_connect_group(group, members, 2) == SRT_ERROR) {
        fprintf(stderr, "group connection failed: %s\n", srt_getlasterror_str());
        return 1;
    }

    SRT_SOCKGROUPDATA state[8];
    size_t state_count = 8;
    for (int attempt = 0; attempt < 100; ++attempt) {
        state_count = 8;
        if (srt_group_data(group, state, &state_count) != SRT_ERROR && state_count >= 2)
            break;
        usleep(20000);
    }
    if (state_count < 2) {
        fprintf(stderr, "expected two connected members, got %zu\n", state_count);
        return 1;
    }

    const char message[] = "restream-srt-bond-test";
    SRT_MSGCTRL message_control;
    srt_msgctrl_init(&message_control);
    if (srt_sendmsg2(group, message, sizeof(message), &message_control) == SRT_ERROR) {
        fprintf(stderr, "group send failed: %s\n", srt_getlasterror_str());
        return 1;
    }

    int failover_tested = 0;
    if (group_type == SRT_GTYPE_BACKUP) {
        SRTSOCKET primary = SRT_INVALID_SOCK;
        for (size_t index = 0; index < state_count; ++index) {
            if (state[index].weight == 1) {
                primary = state[index].id;
                break;
            }
        }
        if (primary == SRT_INVALID_SOCK || srt_close(primary) == SRT_ERROR) {
            fprintf(stderr, "failed to close primary member: %s\n", srt_getlasterror_str());
            return 1;
        }
        usleep(500000);
        srt_msgctrl_init(&message_control);
        if (srt_sendmsg2(group, message, sizeof(message), &message_control) == SRT_ERROR) {
            fprintf(stderr, "backup send after primary close failed: %s\n",
                    srt_getlasterror_str());
            return 1;
        }
        failover_tested = 1;
    }

    printf("connected_group type=%s members=%zu failover=%d\n",
           argv[1], state_count, failover_tested);
    fflush(stdout);
    usleep(500000);
    srt_close(group);
    srt_cleanup();
    return 0;
}
