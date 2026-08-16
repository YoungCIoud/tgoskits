#include <linux/errno.h>
#include <linux/kernel.h>
#include <linux/string.h>
#include <linux/types.h>

#include "includes/message.h"

#define VERSION_OFFSET 0
#define FLAGS_OFFSET 1
#define HEADER_LEN_OFFSET 2
#define FRAGMENT_LEN_OFFSET 4
#define MESSAGE_ID_OFFSET 8
#define MESSAGE_LEN_OFFSET 16

#define KNOWN_FLAGS                                                            \
	(AXIVC_FRAME_FLAG_FIRST | AXIVC_FRAME_FLAG_LAST | AXIVC_FRAME_FLAG_ABORT)

struct axivc_receive_transition;

static int axivc_publish_empty_message(
	struct axivc_message_sender *sender, struct axivc_send_progress *progress);
static int axivc_process_available_cells(
	struct axivc_message_receiver *receiver, u8 *output, size_t output_len,
	struct axivc_receive_progress *progress);
static int
axivc_receiver_fail(struct axivc_message_receiver *receiver, int error);
static int axivc_validate_transition(
	const struct axivc_message_receiver *receiver,
	const struct axivc_decoded_frame *frame,
	struct axivc_receive_transition *transition);

void axivc_message_sender_init(
	struct axivc_message_sender *sender, struct axivc_ring *ring)
{
	sender->ring = ring;
	sender->next_message_id = 1;
	sender->sending = false;
	sender->active.id = 0;
	sender->active.len = 0;
	sender->sent = 0;
	sender->published_any = false;
}

int axivc_message_start(struct axivc_message_sender *sender, u64 message_len)
{
	u64 message_id;

	if (sender->sending)
		return -EBUSY;
	if (sender->next_message_id == 0)
		return -EOVERFLOW;

	message_id = sender->next_message_id;
	/* The id space is deliberately not wrapped: u64::MAX is the last
	 * usable V1 identifier, after which the session must be re-established
	 * instead of risking aliased message ids. */
	if (message_id == ~0ULL)
		sender->next_message_id = 0;
	else
		sender->next_message_id = message_id + 1;

	sender->sending = true;
	sender->active.id = message_id;
	sender->active.len = message_len;
	sender->sent = 0;
	sender->published_any = false;
	return 0;
}

int axivc_message_try_write(
	struct axivc_message_sender *sender, const u8 *input, size_t input_len,
	struct axivc_send_progress *progress)
{
	u64 remaining;
	size_t consumed = 0;
	size_t published_cells = 0;

	progress->consumed = 0;
	progress->published_cells = 0;
	progress->complete = false;

	if (!sender->sending)
		return -ENOMSG;

	remaining = sender->active.len - sender->sent;
	if (input_len > remaining)
		return -EINVAL;

	if (sender->active.len == 0)
		return axivc_publish_empty_message(sender, progress);

	while (consumed < input_len)
	{
		u8 cell[AXIVC_CELL_SIZE];
		size_t fragment_len =
			min_t(size_t, AXIVC_FRAGMENT_CAPACITY, input_len - consumed);
		u64 next_sent = sender->sent + fragment_len;
		bool complete = next_sent == sender->active.len;
		int ret;

		ret = axivc_encode_frame(
			cell, sender->active.id, sender->active.len, !sender->published_any,
			complete, false, input + consumed, fragment_len);
		if (ret)
			return ret;
		if (!axivc_ring_try_push_cell(sender->ring, cell))
			break;

		sender->sent = next_sent;
		sender->published_any = true;
		consumed += fragment_len;
		published_cells += 1;
		if (complete)
		{
			sender->sending = false;
			progress->consumed = consumed;
			progress->published_cells = published_cells;
			progress->complete = true;
			return 0;
		}
	}

	progress->consumed = consumed;
	progress->published_cells = published_cells;
	return 0;
}

int axivc_message_try_abort(struct axivc_message_sender *sender)
{
	u8 cell[AXIVC_CELL_SIZE];
	int ret;

	if (!sender->sending)
		return -ENOMSG;
	if (!sender->published_any)
	{
		sender->sending = false;
		return 0;
	}

	ret = axivc_encode_frame(
		cell, sender->active.id, sender->active.len, false, false, true, NULL,
		0);
	if (ret)
		return ret;
	if (!axivc_ring_try_push_cell(sender->ring, cell))
		return -EAGAIN;

	sender->sending = false;
	return 0;
}

void axivc_message_receiver_init(
	struct axivc_message_receiver *receiver, struct axivc_ring *ring)
{
	receiver->ring = ring;
	receiver->state = AXIVC_RX_IDLE;
	receiver->active.id = 0;
	receiver->active.len = 0;
	receiver->received = 0;
	receiver->failed_errno = 0;
}

void axivc_message_receiver_poison(
	struct axivc_message_receiver *receiver, int error)
{
	receiver->state = AXIVC_RX_FAILED;
	receiver->failed_errno = error;
}

int axivc_message_peek_meta(
	struct axivc_message_receiver *receiver, struct axivc_message_meta *meta,
	bool *available)
{
	u8 cell[AXIVC_CELL_SIZE];
	struct axivc_decoded_frame frame;
	int ret;

	*available = false;
	if (receiver->state == AXIVC_RX_FAILED)
		return receiver->failed_errno;
	if (receiver->state == AXIVC_RX_RECEIVING)
	{
		*meta = receiver->active;
		*available = true;
		return 0;
	}

	if (!axivc_ring_try_peek_cell(receiver->ring, cell))
		return 0;
	ret = axivc_decode_frame(cell, &frame);
	if (ret)
		return axivc_receiver_fail(receiver, ret);
	if (frame.abort || !frame.first)
		return axivc_receiver_fail(receiver, -EPROTO);

	meta->id = frame.message_id;
	meta->len = frame.message_len;
	*available = true;
	return 0;
}

int axivc_message_try_read(
	struct axivc_message_receiver *receiver, u8 *output, size_t output_len,
	struct axivc_receive_progress *progress)
{
	return axivc_process_available_cells(
		receiver, output, output_len, progress);
}

/*
 * Internal protocol engine below this point: empty-message publish, receive
 * state machine, frame codec and validation helpers.
 */

struct axivc_receive_transition
{
	enum axivc_receiver_state next_state;
	struct axivc_message_meta active;
	u64 received;
	bool complete;
	bool aborted;
};

static int axivc_publish_empty_message(
	struct axivc_message_sender *sender, struct axivc_send_progress *progress)
{
	u8 cell[AXIVC_CELL_SIZE];
	int ret = axivc_encode_frame(
		cell, sender->active.id, 0, true, true, false, NULL, 0);

	if (ret)
		return ret;
	if (!axivc_ring_try_push_cell(sender->ring, cell))
		return 0;

	sender->sending = false;
	progress->published_cells = 1;
	progress->complete = true;
	return 0;
}

static int axivc_process_available_cells(
	struct axivc_message_receiver *receiver, u8 *output, size_t output_len,
	struct axivc_receive_progress *progress)
{
	size_t written = 0;
	size_t consumed_cells = 0;

	progress->written = 0;
	progress->consumed_cells = 0;
	progress->complete = false;

	if (receiver->state == AXIVC_RX_FAILED)
		return receiver->failed_errno;

	for (;;)
	{
		u8 cell[AXIVC_CELL_SIZE];
		struct axivc_decoded_frame frame;
		struct axivc_receive_transition transition;
		int ret;

		if (!axivc_ring_try_peek_cell(receiver->ring, cell))
		{
			progress->written = written;
			progress->consumed_cells = consumed_cells;
			return 0;
		}

		ret = axivc_decode_frame(cell, &frame);
		if (ret)
		{
			/* Cells already consumed by this call stay consumed; the
			 * error surfaces again on the next call. */
			if (consumed_cells > 0)
			{
				progress->written = written;
				progress->consumed_cells = consumed_cells;
				return 0;
			}
			return axivc_receiver_fail(receiver, ret);
		}

		ret = axivc_validate_transition(receiver, &frame, &transition);
		if (ret || (transition.aborted && consumed_cells > 0))
		{
			if (consumed_cells > 0)
			{
				progress->written = written;
				progress->consumed_cells = consumed_cells;
				return 0;
			}
			if (ret)
				return axivc_receiver_fail(receiver, ret);
		}

		{
			size_t available = output_len - written;

			if (frame.fragment_len > available)
			{
				if (consumed_cells == 0)
					return -EMSGSIZE;
				progress->written = written;
				progress->consumed_cells = consumed_cells;
				return 0;
			}
			memcpy(output + written, frame.fragment, frame.fragment_len);
			written += frame.fragment_len;
		}

		axivc_ring_pop_cell(receiver->ring);
		consumed_cells += 1;
		receiver->state = transition.next_state;
		receiver->active = transition.active;
		receiver->received = transition.received;

		if (transition.aborted)
			return -ECONNRESET;
		if (transition.complete)
		{
			progress->written = written;
			progress->consumed_cells = consumed_cells;
			progress->complete = true;
			return 0;
		}
	}
}

static int
axivc_receiver_fail(struct axivc_message_receiver *receiver, int error)
{
	receiver->state = AXIVC_RX_FAILED;
	receiver->failed_errno = error;
	return error;
}

static int axivc_validate_transition(
	const struct axivc_message_receiver *receiver,
	const struct axivc_decoded_frame *frame,
	struct axivc_receive_transition *transition)
{
	struct axivc_message_meta active;
	u64 received;

	switch (receiver->state)
	{
	case AXIVC_RX_IDLE:
		if (frame->abort || !frame->first)
			return -EPROTO;
		active.id = frame->message_id;
		active.len = frame->message_len;
		received = 0;
		break;
	case AXIVC_RX_RECEIVING:
		if (frame->first)
			return -EPROTO;
		if (frame->message_id != receiver->active.id)
			return -EPROTO;
		if (frame->message_len != receiver->active.len)
			return -EPROTO;
		active = receiver->active;
		received = receiver->received;
		break;
	default:
		return receiver->failed_errno;
	}

	transition->complete = false;
	transition->aborted = false;

	if (frame->abort)
	{
		transition->next_state = AXIVC_RX_IDLE;
		transition->active = active;
		transition->received = received;
		transition->aborted = true;
		return 0;
	}

	if (received > ~0ULL - frame->fragment_len)
		return -EPROTO;
	received += frame->fragment_len;
	if (received > active.len)
		return -EPROTO;
	if (frame->last && received != active.len)
		return -EPROTO;

	transition->next_state = frame->last ? AXIVC_RX_IDLE : AXIVC_RX_RECEIVING;
	transition->active = active;
	transition->received = received;
	transition->complete = frame->last;
	return 0;
}

static u16 axivc_read_le16(const u8 *p) { return (u16)p[0] | ((u16)p[1] << 8); }

static u32 axivc_read_le32(const u8 *p)
{
	return (u32)p[0] | ((u32)p[1] << 8) | ((u32)p[2] << 16) | ((u32)p[3] << 24);
}

static u64 axivc_read_le64(const u8 *p)
{
	return (u64)axivc_read_le32(p) | ((u64)axivc_read_le32(p + 4) << 32);
}

static void axivc_write_le16(u8 *p, u16 value)
{
	p[0] = value & 0xff;
	p[1] = (value >> 8) & 0xff;
}

static void axivc_write_le32(u8 *p, u32 value)
{
	p[0] = value & 0xff;
	p[1] = (value >> 8) & 0xff;
	p[2] = (value >> 16) & 0xff;
	p[3] = (value >> 24) & 0xff;
}

static void axivc_write_le64(u8 *p, u64 value)
{
	axivc_write_le32(p, value & 0xffffffffULL);
	axivc_write_le32(p + 4, value >> 32);
}

/* Validates the flag/payload shape shared by outgoing and incoming frames:
 * ABORT carries no payload and no FIRST/LAST; an empty logical message is
 * exactly one FIRST|LAST frame without payload; every other frame carries at
 * least one payload byte. */
static int axivc_validate_frame_shape(
	u64 message_len, size_t fragment_len, bool first, bool last, bool abort)
{
	if (abort)
	{
		if (first || last || fragment_len != 0)
			return -EPROTO;
		return 0;
	}

	if (message_len == 0)
	{
		if (!first || !last || fragment_len != 0)
			return -EPROTO;
	}
	else if (fragment_len == 0)
	{
		return -EPROTO;
	}
	return 0;
}

int axivc_encode_frame(
	u8 cell[AXIVC_CELL_SIZE], u64 message_id, u64 message_len, bool first,
	bool last, bool abort, const u8 *fragment, size_t fragment_len)
{
	u8 flags = 0;
	int ret;

	if (message_id == 0 || fragment_len > AXIVC_FRAGMENT_CAPACITY)
		return -EPROTO;
	ret = axivc_validate_frame_shape(
		message_len, fragment_len, first, last, abort);
	if (ret)
		return ret;

	if (first)
		flags |= AXIVC_FRAME_FLAG_FIRST;
	if (last)
		flags |= AXIVC_FRAME_FLAG_LAST;
	if (abort)
		flags |= AXIVC_FRAME_FLAG_ABORT;

	memset(cell, 0, AXIVC_CELL_SIZE);
	cell[VERSION_OFFSET] = AXIVC_MESSAGE_VERSION_V1;
	cell[FLAGS_OFFSET] = flags;
	axivc_write_le16(cell + HEADER_LEN_OFFSET, AXIVC_V1_HEADER_LEN);
	axivc_write_le32(cell + FRAGMENT_LEN_OFFSET, fragment_len);
	axivc_write_le64(cell + MESSAGE_ID_OFFSET, message_id);
	axivc_write_le64(cell + MESSAGE_LEN_OFFSET, message_len);
	if (fragment_len > 0)
		memcpy(cell + AXIVC_V1_HEADER_LEN, fragment, fragment_len);
	return 0;
}

int axivc_decode_frame(
	const u8 cell[AXIVC_CELL_SIZE], struct axivc_decoded_frame *frame)
{
	u8 version = cell[VERSION_OFFSET];
	u8 flags = cell[FLAGS_OFFSET];
	u16 header_len;
	u32 fragment_len;
	u64 message_id;
	bool first, last, abort;
	int ret;

	if (version != AXIVC_MESSAGE_VERSION_V1)
		return -EPROTONOSUPPORT;
	if (flags & ~KNOWN_FLAGS)
		return -EPROTO;

	header_len = axivc_read_le16(cell + HEADER_LEN_OFFSET);
	if (header_len != AXIVC_V1_HEADER_LEN)
		return -EPROTO;

	fragment_len = axivc_read_le32(cell + FRAGMENT_LEN_OFFSET);
	if (fragment_len > AXIVC_CELL_SIZE - header_len)
		return -EPROTO;

	message_id = axivc_read_le64(cell + MESSAGE_ID_OFFSET);
	if (message_id == 0)
		return -EPROTO;

	first = flags & AXIVC_FRAME_FLAG_FIRST;
	last = flags & AXIVC_FRAME_FLAG_LAST;
	abort = flags & AXIVC_FRAME_FLAG_ABORT;
	ret = axivc_validate_frame_shape(
		axivc_read_le64(cell + MESSAGE_LEN_OFFSET), fragment_len, first, last,
		abort);
	if (ret)
		return ret;

	frame->message_id = message_id;
	frame->message_len = axivc_read_le64(cell + MESSAGE_LEN_OFFSET);
	frame->fragment = cell + header_len;
	frame->fragment_len = fragment_len;
	frame->first = first;
	frame->last = last;
	frame->abort = abort;
	return 0;
}
