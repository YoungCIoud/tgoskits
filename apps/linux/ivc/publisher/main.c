#include <ivc/app_proto.h>
#include <ivc/ulib.h>

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define DEFAULT_CHANNEL_SIZE 0x10000UL
#define MAX_EMPTY_POLLS 200000U

static int send_requests(ivc_publisher_p publisher)
{
    uint8_t payload[IVC_APP_MAX_MESSAGE_LEN];

    for (size_t index = 0; index < IVC_APP_REQUEST_COUNT; index++) {
        uint64_t sequence = index + 1;
        size_t message_len = ivc_app_request_lengths[index];

        if (!ivc_app_encode_pattern(
                payload,
                message_len,
                IVC_APP_MESSAGE_REQUEST,
                sequence)) {
            fprintf(stderr,
                    "linux ivc validation failed: cannot encode request seq=%llu\n",
                    (unsigned long long)sequence);
            return -1;
        }
        if (ivc_write_all(publisher, payload, message_len) !=
            (int)message_len) {
            fprintf(stderr,
                    "linux ivc send error: request seq=%llu len=%zu\n",
                    (unsigned long long)sequence,
                    message_len);
            return -1;
        }
        printf("linux ivc send seq=%llu len=%zu\n",
               (unsigned long long)sequence,
               message_len);
    }
    return 0;
}

static int receive_replies(ivc_publisher_p publisher)
{
    uint8_t payload[IVC_APP_MAX_MESSAGE_LEN];
    uint64_t expected_ack_sequence = 1;
    uint64_t expected_data_sequence = 1;
    unsigned long empty_polls = 0;

    while (expected_ack_sequence <= IVC_APP_REQUEST_COUNT) {
        struct ivc_app_message message;
        int bytes_read =
            ivc_publisher_read(publisher, payload, sizeof(payload));

        if (bytes_read < 0) {
            fprintf(stderr, "linux ivc recv error: reply read failed\n");
            return -1;
        }
        if (bytes_read == 0) {
            if (++empty_polls > MAX_EMPTY_POLLS) {
                fprintf(stderr,
                        "linux ivc recv error: timed out waiting for replies\n");
                return -1;
            }
            usleep(10000);
            continue;
        }
        empty_polls = 0;

        if (!ivc_app_decode(payload, (size_t)bytes_read, &message)) {
            fprintf(stderr,
                    "linux ivc recv error: malformed application payload\n");
            return -1;
        }

        switch (message.kind) {
        case IVC_APP_MESSAGE_ACK:
            if (message.sequence != expected_ack_sequence ||
                !ivc_app_ack_body_is_valid(message.body, message.body_len)) {
                fprintf(stderr,
                        "linux ivc validation failed: ack expected=%llu actual=%llu len=%zu\n",
                        (unsigned long long)expected_ack_sequence,
                        (unsigned long long)message.sequence,
                        message.body_len);
                return -1;
            }
            if (expected_ack_sequence == IVC_APP_REQUEST_COUNT &&
                expected_data_sequence != IVC_APP_DATA_COUNT + 1) {
                fprintf(stderr,
                        "linux ivc validation failed: missing subscriber data expected=%llu\n",
                        (unsigned long long)expected_data_sequence);
                return -1;
            }
            printf("linux ivc ack seq=%llu msg=%.*s\n",
                   (unsigned long long)message.sequence,
                   (int)message.body_len,
                   (const char *)message.body);
            expected_ack_sequence++;
            break;
        case IVC_APP_MESSAGE_DATA: {
            size_t index;
            size_t expected_len;

            if (expected_data_sequence > IVC_APP_DATA_COUNT) {
                fprintf(stderr,
                        "linux ivc validation failed: unexpected data seq=%llu\n",
                        (unsigned long long)message.sequence);
                return -1;
            }
            index = (size_t)(expected_data_sequence - 1);
            expected_len = ivc_app_data_lengths[index];
            if (!ivc_app_validate_pattern(
                    &message,
                    IVC_APP_MESSAGE_DATA,
                    expected_data_sequence,
                    expected_len)) {
                fprintf(stderr,
                        "linux ivc validation failed: data expected=%llu actual=%llu len=%d\n",
                        (unsigned long long)expected_data_sequence,
                        (unsigned long long)message.sequence,
                        bytes_read);
                return -1;
            }
            printf("linux ivc recv data seq=%llu len=%d\n",
                   (unsigned long long)message.sequence,
                   bytes_read);
            expected_data_sequence++;
            break;
        }
        case IVC_APP_MESSAGE_REQUEST:
        default:
            fprintf(stderr,
                    "linux ivc validation failed: unexpected request on reply ring\n");
            return -1;
        }
    }
    return 0;
}

static void print_usage(const char *program)
{
    fprintf(stderr, "Usage: %s <channel_key> [channel_size]\n", program);
}

int main(int argc, char *argv[])
{
    unsigned long channel_size = DEFAULT_CHANNEL_SIZE;
    int ret = 0;
    ivc_manager_p manager;
    ivc_publisher_p publisher;

    if (argc < 2 || argc > 3) {
        print_usage(argv[0]);
        return 1;
    }

    uint64_t channel_key = strtoull(argv[1], NULL, 0);
    if (argc == 3) {
        channel_size = strtoul(argv[2], NULL, 0);
    }

    manager = ivc_open_manager();
    if (!manager) {
        fprintf(stderr, "Failed to open IVC manager\n");
        return 1;
    }

    publisher = ivc_publish(manager, channel_key, channel_size);
    if (!publisher) {
        fprintf(stderr, "Failed to publish channel\n");
        ret = 2;
        goto close_manager;
    }

    if (send_requests(publisher) != 0 || receive_replies(publisher) != 0) {
        ret = 3;
    }

    if (ivc_unpublish(publisher) < 0) {
        fprintf(stderr, "Failed to unpublish channel\n");
        ret = 4;
    }
close_manager:
    if (ivc_close_manager(manager) < 0) {
        fprintf(stderr, "Failed to close IVC manager\n");
        ret = 5;
    }
    if (ret == 0) {
        printf("linux ivc publisher pass\n");
    }
    printf("IVC publisher example finished.\n");
    return ret;
}
