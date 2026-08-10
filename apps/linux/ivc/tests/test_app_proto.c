#include <assert.h>
#include <stdio.h>
#include <string.h>

#include <ivc/app_proto.h>

static void test_header_uses_little_endian_layout(void)
{
    static const uint8_t expected[] = {
        1, 1, 2, 3, 4, 5, 6, 7, 8, 3, 0, 'a', 'b', 'c',
    };
    struct ivc_app_message message;
    uint8_t payload[sizeof(expected)];
    size_t message_len;

    assert(ivc_app_encode_text(
        payload,
        sizeof(payload),
        IVC_APP_MESSAGE_REQUEST,
        0x0807060504030201ULL,
        (const uint8_t *)"abc",
        3,
        &message_len));
    assert(message_len == sizeof(expected));
    assert(memcmp(payload, expected, sizeof(expected)) == 0);
    assert(ivc_app_decode(payload, sizeof(payload), &message));
    assert(message.kind == IVC_APP_MESSAGE_REQUEST);
    assert(message.sequence == 0x0807060504030201ULL);
    assert(message.body_len == 3);
}

static void test_pattern_messages_match_all_demo_boundaries(void)
{
    struct ivc_app_message message;
    uint8_t payload[IVC_APP_MAX_MESSAGE_LEN];

    for (size_t index = 0; index < IVC_APP_REQUEST_COUNT; index++) {
        size_t len = ivc_app_request_lengths[index];
        uint64_t sequence = index + 1;

        assert(ivc_app_encode_pattern(
            payload, len, IVC_APP_MESSAGE_REQUEST, sequence));
        assert(ivc_app_decode(payload, len, &message));
        assert(ivc_app_validate_pattern(
            &message, IVC_APP_MESSAGE_REQUEST, sequence, len));
    }

    for (size_t index = 0; index < IVC_APP_DATA_COUNT; index++) {
        size_t len = ivc_app_data_lengths[index];
        uint64_t sequence = index + 1;

        assert(ivc_app_encode_pattern(
            payload, len, IVC_APP_MESSAGE_DATA, sequence));
        assert(ivc_app_decode(payload, len, &message));
        assert(ivc_app_validate_pattern(
            &message, IVC_APP_MESSAGE_DATA, sequence, len));
    }
}

static void test_decode_rejects_malformed_kind_and_length(void)
{
    struct ivc_app_message message;
    uint8_t payload[39];

    assert(ivc_app_encode_pattern(
        payload, sizeof(payload), IVC_APP_MESSAGE_REQUEST, 1));
    payload[0] = 0xff;
    assert(!ivc_app_decode(payload, sizeof(payload), &message));

    assert(ivc_app_encode_pattern(
        payload, sizeof(payload), IVC_APP_MESSAGE_REQUEST, 1));
    payload[9]++;
    assert(!ivc_app_decode(payload, sizeof(payload), &message));
}

static void test_validation_rejects_corrupted_pattern_body(void)
{
    struct ivc_app_message message;
    uint8_t payload[41];

    assert(ivc_app_encode_pattern(
        payload, sizeof(payload), IVC_APP_MESSAGE_DATA, 1));
    payload[IVC_APP_HEADER_LEN] ^= 1;
    assert(ivc_app_decode(payload, sizeof(payload), &message));
    assert(!ivc_app_validate_pattern(
        &message, IVC_APP_MESSAGE_DATA, 1, sizeof(payload)));
}

static void test_encoders_reject_invalid_bounds(void)
{
    uint8_t payload[IVC_APP_HEADER_LEN];
    size_t message_len;

    assert(!ivc_app_encode_pattern(
        payload,
        IVC_APP_HEADER_LEN - 1,
        IVC_APP_MESSAGE_REQUEST,
        1));
    assert(!ivc_app_encode_text(
        payload,
        sizeof(payload),
        IVC_APP_MESSAGE_ACK,
        1,
        (const uint8_t *)"x",
        1,
        &message_len));
}

static void test_ack_body_preserves_peer_identity(void)
{
    assert(ivc_app_ack_body_is_valid(
        (const uint8_t *)"ack from linux subscriber",
        strlen("ack from linux subscriber")));
    assert(ivc_app_ack_body_is_valid(
        (const uint8_t *)"ack from arceos subscriber",
        strlen("ack from arceos subscriber")));
    assert(!ivc_app_ack_body_is_valid(
        (const uint8_t *)"ack from subscriber",
        strlen("ack from subscriber")));
    assert(!ivc_app_ack_body_is_valid(
        (const uint8_t *)"ack from unknown subscriber",
        strlen("ack from unknown subscriber")));
    assert(!ivc_app_ack_body_is_valid(
        (const uint8_t *)"ack from linux publisher",
        strlen("ack from linux publisher")));
}

int main(void)
{
    test_header_uses_little_endian_layout();
    test_pattern_messages_match_all_demo_boundaries();
    test_decode_rejects_malformed_kind_and_length();
    test_validation_rejects_corrupted_pattern_body();
    test_encoders_reject_invalid_bounds();
    test_ack_body_preserves_peer_identity();
    puts("Linux IVC application protocol tests passed");
    return 0;
}
