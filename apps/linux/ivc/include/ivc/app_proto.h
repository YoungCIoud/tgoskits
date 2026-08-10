#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define IVC_APP_HEADER_LEN 11U
#define IVC_APP_MAX_MESSAGE_LEN 700U
#define IVC_APP_REQUEST_COUNT 5U
#define IVC_APP_DATA_COUNT 3U

extern const size_t ivc_app_request_lengths[IVC_APP_REQUEST_COUNT];
extern const size_t ivc_app_data_lengths[IVC_APP_DATA_COUNT];

enum ivc_app_message_kind {
    IVC_APP_MESSAGE_REQUEST = 1,
    IVC_APP_MESSAGE_ACK = 2,
    IVC_APP_MESSAGE_DATA = 3,
};

struct ivc_app_message {
    enum ivc_app_message_kind kind;
    uint64_t sequence;
    const uint8_t *body;
    size_t body_len;
};

/* Encodes the 11-byte demo application header followed by a deterministic
 * body pattern shared with the ArceOS publisher/subscriber. */
bool ivc_app_encode_pattern(
    uint8_t *payload,
    size_t payload_len,
    enum ivc_app_message_kind kind,
    uint64_t sequence);

/* Encodes one text-body application message, used for acknowledgements. */
bool ivc_app_encode_text(
    uint8_t *payload,
    size_t payload_capacity,
    enum ivc_app_message_kind kind,
    uint64_t sequence,
    const uint8_t *body,
    size_t body_len,
    size_t *message_len);

/* Decodes and validates the application header and exact body length. */
bool ivc_app_decode(
    const uint8_t *payload,
    size_t payload_len,
    struct ivc_app_message *message);

/* Validates kind, sequence, total length and deterministic body bytes. */
bool ivc_app_validate_pattern(
    const struct ivc_app_message *message,
    enum ivc_app_message_kind expected_kind,
    uint64_t expected_sequence,
    size_t expected_len);

/* Accepts the known ArceOS and Linux subscriber acknowledgement bodies used
 * as cross-OS test markers. */
bool ivc_app_ack_body_is_valid(const uint8_t *body, size_t body_len);

#ifdef __cplusplus
}
#endif
