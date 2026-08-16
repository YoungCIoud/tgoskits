#pragma once

#include <linux/types.h>

#include "ring.h"

/*
 * Message V1 framing over opaque ring cells.
 *
 * One logical message is encoded into one or more fixed-size cells. Every
 * cell carries a 24-byte little-endian header followed by up to
 * AXIVC_FRAGMENT_CAPACITY payload bytes:
 *
 *   offset  size  field
 *   0x00    1     version       (1)
 *   0x01    1     flags         (FIRST | LAST | ABORT)
 *   0x02    2     header_len    (24)
 *   0x04    4     fragment_len
 *   0x08    8     message_id    (nonzero, per direction, starts at 1)
 *   0x10    8     message_len   (total logical message payload length)
 *   0x18    ..    fragment bytes
 *
 * Application semantics (Request/Ack, RPC headers, file chunks) live in the
 * payload, not in this layer. The state machines below are a C port of the
 * Rust axivc IvcMessageSender/IvcMessageReceiver and must keep the same
 * externally observable behavior: no interleaving, validate before consume,
 * poison on protocol errors.
 */
#define AXIVC_MESSAGE_VERSION_V1 1U
#define AXIVC_V1_HEADER_LEN 24U
#define AXIVC_FRAGMENT_CAPACITY (AXIVC_CELL_SIZE - AXIVC_V1_HEADER_LEN)

#define AXIVC_FRAME_FLAG_FIRST (1U << 0)
#define AXIVC_FRAME_FLAG_LAST (1U << 1)
#define AXIVC_FRAME_FLAG_ABORT (1U << 2)

/*
 * Error mapping used by the message layer:
 *
 *   -EBUSY             start/abort while another message is in progress
 *   -EOVERFLOW         per-direction message id space exhausted (u64::MAX)
 *   -ENOMSG            write/abort without an active message
 *   -EINVAL            input longer than the declared unsent payload
 *   -EAGAIN            ABORT cell required but the ring is full
 *   -EPROTONOSUPPORT   frame version is not V1
 *   -EPROTO            malformed or inconsistent frame / protocol violation
 *   -EMSGSIZE          output buffer cannot hold the next fragment
 *   -ECONNRESET        peer aborted the in-flight message
 *
 * Every protocol error poisons the receiver: later calls keep returning the
 * same error because V1 has no reliable resynchronization marker.
 */

struct axivc_message_meta
{
	u64 id;
	u64 len;
};

struct axivc_message_sender
{
	struct axivc_ring *ring;
	u64 next_message_id; /* 0 once the id space is exhausted */
	bool sending;
	struct axivc_message_meta active;
	u64 sent;
	bool published_any;
};

struct axivc_send_progress
{
	size_t consumed;
	size_t published_cells;
	bool complete;
};

enum axivc_receiver_state
{
	AXIVC_RX_IDLE,
	AXIVC_RX_RECEIVING,
	AXIVC_RX_FAILED,
};

struct axivc_message_receiver
{
	struct axivc_ring *ring;
	enum axivc_receiver_state state;
	struct axivc_message_meta active;
	u64 received;
	int failed_errno;
};

struct axivc_receive_progress
{
	size_t written;
	size_t consumed_cells;
	bool complete;
};

void axivc_message_sender_init(
	struct axivc_message_sender *sender, struct axivc_ring *ring);

/* Establishes local send state only; the first cell is published by
 * axivc_message_try_write(). */
int axivc_message_start(struct axivc_message_sender *sender, u64 message_len);

/* Publishes as many complete fragment cells as ring space allows. Ring-full
 * backpressure is reported as successful progress with complete == false. An
 * empty message is published by calling this with input_len == 0 after
 * axivc_message_start(0). */
int axivc_message_try_write(
	struct axivc_message_sender *sender, const u8 *input, size_t input_len,
	struct axivc_send_progress *progress);

/* Cancels the active message. Local-only when nothing was published yet;
 * otherwise publishes one ABORT cell. Returns -EAGAIN when the ABORT cell
 * does not fit, leaving the send active so the caller can retry. */
int axivc_message_try_abort(struct axivc_message_sender *sender);

void axivc_message_receiver_init(
	struct axivc_message_receiver *receiver, struct axivc_ring *ring);

/* Marks the endpoint unrecoverable after an OS-adapter failure that occurred
 * after cells were consumed (for example, a user-copy fault or mid-message
 * timeout). Later operations return the same error instead of delivering a
 * suffix as a new successful message. */
void axivc_message_receiver_poison(
	struct axivc_message_receiver *receiver, int error);

/* Reports metadata of the current or next message without consuming its
 * first cell, so the caller can enforce its own size policy before reading.
 * *available is false when no cell is published yet. */
int axivc_message_peek_meta(
	struct axivc_message_receiver *receiver, struct axivc_message_meta *meta,
	bool *available);

/* Copies as many complete fragments as fit in output. When the next
 * fragment does not fit and nothing was copied by this call, returns
 * -EMSGSIZE and leaves the cell at the ring head. An output buffer of at
 * least AXIVC_FRAGMENT_CAPACITY bytes always makes progress when a cell is
 * available. */
int axivc_message_try_read(
	struct axivc_message_receiver *receiver, u8 *output, size_t output_len,
	struct axivc_receive_progress *progress);

struct axivc_decoded_frame
{
	u64 message_id;
	u64 message_len;
	const u8 *fragment;
	size_t fragment_len;
	bool first;
	bool last;
	bool abort;
};

/* Raw frame codec, exposed so conformance tests can drive wire-format
 * vectors without standing up a ring. Protocol peers must use the sender /
 * receiver state machines above instead of hand-encoding frames. */
int axivc_encode_frame(
	u8 cell[AXIVC_CELL_SIZE], u64 message_id, u64 message_len, bool first,
	bool last, bool abort, const u8 *fragment, size_t fragment_len);
int axivc_decode_frame(
	const u8 cell[AXIVC_CELL_SIZE], struct axivc_decoded_frame *frame);
