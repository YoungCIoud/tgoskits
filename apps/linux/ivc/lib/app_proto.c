#include <ivc/app_proto.h>

#include <limits.h>
#include <string.h>

const size_t ivc_app_request_lengths[IVC_APP_REQUEST_COUNT] = {
    39, 40, 41, 640, 700,
};
const size_t ivc_app_data_lengths[IVC_APP_DATA_COUNT] = {
    41, 641, 700,
};

static bool ivc_app_kind_is_valid(enum ivc_app_message_kind kind)
{
    return kind == IVC_APP_MESSAGE_REQUEST ||
           kind == IVC_APP_MESSAGE_ACK ||
           kind == IVC_APP_MESSAGE_DATA;
}

static void ivc_app_write_le16(uint8_t *output, uint16_t value)
{
    output[0] = (uint8_t)value;
    output[1] = (uint8_t)(value >> 8);
}

static void ivc_app_write_le64(uint8_t *output, uint64_t value)
{
    for (size_t index = 0; index < sizeof(value); index++) {
        output[index] = (uint8_t)(value >> (index * CHAR_BIT));
    }
}

static uint16_t ivc_app_read_le16(const uint8_t *input)
{
    return (uint16_t)input[0] | ((uint16_t)input[1] << 8);
}

static uint64_t ivc_app_read_le64(const uint8_t *input)
{
    uint64_t value = 0;

    for (size_t index = 0; index < sizeof(value); index++) {
        value |= (uint64_t)input[index] << (index * CHAR_BIT);
    }
    return value;
}

static void ivc_app_encode_header(
    uint8_t *payload,
    enum ivc_app_message_kind kind,
    uint64_t sequence,
    uint16_t body_len)
{
    payload[0] = (uint8_t)kind;
    ivc_app_write_le64(payload + 1, sequence);
    ivc_app_write_le16(payload + 9, body_len);
}

static uint8_t ivc_app_pattern_byte(
    enum ivc_app_message_kind kind,
    uint64_t sequence,
    size_t index)
{
    uint8_t pattern_index =
        (uint8_t)index + (uint8_t)sequence + (uint8_t)kind * 7U;

    return (uint8_t)('a' + pattern_index % 26U);
}

bool ivc_app_encode_pattern(
    uint8_t *payload,
    size_t payload_len,
    enum ivc_app_message_kind kind,
    uint64_t sequence)
{
    size_t body_len;

    if (!payload || !ivc_app_kind_is_valid(kind) ||
        payload_len < IVC_APP_HEADER_LEN) {
        return false;
    }
    body_len = payload_len - IVC_APP_HEADER_LEN;
    if (body_len > UINT16_MAX) {
        return false;
    }

    ivc_app_encode_header(payload, kind, sequence, (uint16_t)body_len);
    for (size_t index = 0; index < body_len; index++) {
        payload[IVC_APP_HEADER_LEN + index] =
            ivc_app_pattern_byte(kind, sequence, index);
    }
    return true;
}

bool ivc_app_encode_text(
    uint8_t *payload,
    size_t payload_capacity,
    enum ivc_app_message_kind kind,
    uint64_t sequence,
    const uint8_t *body,
    size_t body_len,
    size_t *message_len)
{
    size_t required;

    if (!payload || !message_len || !ivc_app_kind_is_valid(kind) ||
        body_len > UINT16_MAX || (body_len > 0 && !body)) {
        return false;
    }
    if (body_len > SIZE_MAX - IVC_APP_HEADER_LEN) {
        return false;
    }
    required = IVC_APP_HEADER_LEN + body_len;
    if (required > payload_capacity) {
        return false;
    }

    ivc_app_encode_header(payload, kind, sequence, (uint16_t)body_len);
    if (body_len > 0) {
        memcpy(payload + IVC_APP_HEADER_LEN, body, body_len);
    }
    *message_len = required;
    return true;
}

bool ivc_app_decode(
    const uint8_t *payload,
    size_t payload_len,
    struct ivc_app_message *message)
{
    enum ivc_app_message_kind kind;
    size_t body_len;

    if (!payload || !message || payload_len < IVC_APP_HEADER_LEN) {
        return false;
    }

    kind = (enum ivc_app_message_kind)payload[0];
    if (!ivc_app_kind_is_valid(kind)) {
        return false;
    }
    body_len = ivc_app_read_le16(payload + 9);
    if (body_len != payload_len - IVC_APP_HEADER_LEN) {
        return false;
    }

    message->kind = kind;
    message->sequence = ivc_app_read_le64(payload + 1);
    message->body = payload + IVC_APP_HEADER_LEN;
    message->body_len = body_len;
    return true;
}

bool ivc_app_validate_pattern(
    const struct ivc_app_message *message,
    enum ivc_app_message_kind expected_kind,
    uint64_t expected_sequence,
    size_t expected_len)
{
    if (!message || message->kind != expected_kind ||
        message->sequence != expected_sequence ||
        expected_len < IVC_APP_HEADER_LEN ||
        message->body_len != expected_len - IVC_APP_HEADER_LEN) {
        return false;
    }

    for (size_t index = 0; index < message->body_len; index++) {
        if (message->body[index] !=
            ivc_app_pattern_byte(expected_kind, expected_sequence, index)) {
            return false;
        }
    }
    return true;
}

bool ivc_app_ack_body_is_valid(const uint8_t *body, size_t body_len)
{
    static const uint8_t arceos_ack[] = "ack from arceos subscriber";
    static const uint8_t linux_ack[] = "ack from linux subscriber";

    if (!body) {
        return false;
    }
    return (body_len == sizeof(arceos_ack) - 1 &&
            memcmp(body, arceos_ack, body_len) == 0) ||
           (body_len == sizeof(linux_ack) - 1 &&
            memcmp(body, linux_ack, body_len) == 0);
}
