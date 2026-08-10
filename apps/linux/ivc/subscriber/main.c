#include <ivc/app_proto.h>
#include <ivc/ulib.h>

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define MAX_EMPTY_POLLS 200000U

static const uint8_t ACK_BODY[] = "ack from linux subscriber";

static int send_data_messages(ivc_subscriber_p subscriber)
{
    uint8_t payload[IVC_APP_MAX_MESSAGE_LEN];

    for (size_t index = 0; index < IVC_APP_DATA_COUNT; index++) {
        uint64_t sequence = index + 1;
        size_t message_len = ivc_app_data_lengths[index];

        if (!ivc_app_encode_pattern(
                payload,
                message_len,
                IVC_APP_MESSAGE_DATA,
                sequence)) {
            fprintf(stderr,
                    "linux ivc validation failed: cannot encode data seq=%llu\n",
                    (unsigned long long)sequence);
            return -1;
        }
        if (ivc_subscriber_write_all(subscriber, payload, message_len) !=
            (int)message_len) {
            fprintf(stderr,
                    "linux ivc send error: data seq=%llu len=%zu\n",
                    (unsigned long long)sequence,
                    message_len);
            return -1;
        }
        printf("linux ivc send data seq=%llu len=%zu\n",
               (unsigned long long)sequence,
               message_len);
    }
    return 0;
}

static int send_ack(ivc_subscriber_p subscriber, uint64_t sequence)
{
    uint8_t payload[IVC_APP_HEADER_LEN + sizeof(ACK_BODY) - 1];
    size_t message_len;

    if (!ivc_app_encode_text(
            payload,
            sizeof(payload),
            IVC_APP_MESSAGE_ACK,
            sequence,
            ACK_BODY,
            sizeof(ACK_BODY) - 1,
            &message_len)) {
        fprintf(stderr,
                "linux ivc validation failed: cannot encode ack seq=%llu\n",
                (unsigned long long)sequence);
        return -1;
    }
    if (ivc_subscriber_write_all(subscriber, payload, message_len) !=
        (int)message_len) {
        fprintf(stderr,
                "linux ivc send error: ack seq=%llu\n",
                (unsigned long long)sequence);
        return -1;
    }
    printf("linux ivc ack pub seq=%llu\n", (unsigned long long)sequence);
    return 0;
}

static int receive_requests(ivc_subscriber_p subscriber)
{
    uint8_t payload[IVC_APP_MAX_MESSAGE_LEN];
    size_t received = 0;
    unsigned long empty_polls = 0;

    while (received < IVC_APP_REQUEST_COUNT) {
        struct ivc_app_message message;
        uint64_t expected_sequence = received + 1;
        size_t expected_len = ivc_app_request_lengths[received];
        int bytes_read = ivc_read(subscriber, payload, sizeof(payload));

        if (bytes_read < 0) {
            fprintf(stderr, "linux ivc recv error: request read failed\n");
            return -1;
        }
        if (bytes_read == 0) {
            if (++empty_polls > MAX_EMPTY_POLLS) {
                fprintf(stderr,
                        "linux ivc recv error: timed out waiting for requests\n");
                return -1;
            }
            usleep(10000);
            continue;
        }

        empty_polls = 0;
        if (!ivc_app_decode(payload, (size_t)bytes_read, &message) ||
            !ivc_app_validate_pattern(
                &message,
                IVC_APP_MESSAGE_REQUEST,
                expected_sequence,
                expected_len)) {
            fprintf(stderr,
                    "linux ivc validation failed: request expected=%llu len=%d\n",
                    (unsigned long long)expected_sequence,
                    bytes_read);
            return -1;
        }

        printf("linux ivc recv request seq=%llu len=%d\n",
               (unsigned long long)message.sequence,
               bytes_read);
        if (send_ack(subscriber, message.sequence) != 0) {
            return -1;
        }
        received++;
    }
    return 0;
}

static void print_usage(const char *program)
{
    fprintf(stderr,
            "Usage: %s <target_publisher_id> <channel_key> [message_count]\n",
            program);
}

int main(int argc, char *argv[])
{
    unsigned long target_count = IVC_APP_REQUEST_COUNT;
    int ret = 0;
    ivc_manager_p manager;
    ivc_subscriber_p subscriber;

    if (argc != 3 && argc != 4) {
        print_usage(argv[0]);
        return 1;
    }

    uint64_t target_publisher_id = strtoull(argv[1], NULL, 0);
    uint64_t channel_key = strtoull(argv[2], NULL, 0);
    if (argc == 4) {
        target_count = strtoul(argv[3], NULL, 0);
    }
    if (target_count != IVC_APP_REQUEST_COUNT) {
        fprintf(stderr,
                "Message V1 demo requires exactly %u requests\n",
                IVC_APP_REQUEST_COUNT);
        return 1;
    }

    manager = ivc_open_manager();
    if (!manager) {
        fprintf(stderr, "Failed to open IVC manager\n");
        return 1;
    }

    subscriber =
        ivc_subscribe(manager, target_publisher_id, channel_key);
    if (!subscriber) {
        fprintf(stderr, "Failed to subscribe to channel\n");
        ret = 2;
        goto close_manager;
    }

    /* Match the ArceOS subscriber: publish all independently sequenced Data
     * messages before the fifth Ack, while the peer concurrently drains the
     * reply ring. This proves full-duplex progress and ring backpressure. */
    if (send_data_messages(subscriber) != 0 ||
        receive_requests(subscriber) != 0) {
        ret = 3;
    }

    if (ivc_unsubscribe(subscriber) < 0) {
        fprintf(stderr, "Failed to unsubscribe from channel\n");
        ret = 4;
    }
close_manager:
    if (ivc_close_manager(manager) < 0) {
        fprintf(stderr, "Failed to close IVC manager\n");
        ret = 5;
    }
    if (ret == 0) {
        printf("linux ivc demo pass\n");
    }
    printf("IVC subscriber example finished.\n");
    return ret;
}
