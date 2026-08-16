#pragma once

#include <linux/compiler.h>
#include <linux/types.h>

/* Opaque-cell SPSC ring shared with the Rust `axivc` crate. */
#define AXIVC_CELL_SIZE 64U
#define AXIVC_RING_CAPACITY 16U

#define AXIVC_RING_DIRECTION_PUBLISHER_TO_SUBSCRIBER 1U
#define AXIVC_RING_DIRECTION_SUBSCRIBER_TO_PUBLISHER 2U

/*
 * Single-producer, single-consumer opaque-cell ring.
 *
 * The ring never interprets cell contents; only head/tail carry
 * synchronization. Producer and consumer synchronize through acquire/release
 * on head/tail exactly like the Rust peer:
 *
 * - producer reads head with acquire, writes the full cell, then publishes
 *   tail with release;
 * - consumer reads tail with acquire, copies the cell out, then releases
 *   head.
 *
 * cells must start at offset 64 inside the ring so every cell stays
 * 64-byte aligned; sizeof(struct axivc_ring) is 1088.
 */
struct axivc_ring
{
	u32 direction;
	u32 capacity;
	u32 cell_size;
	u32 head;
	u32 tail;
	u32 reserved[3];
	u8 cells[AXIVC_RING_CAPACITY][AXIVC_CELL_SIZE] __aligned(64);
} __aligned(64);

/* Resets the ring to an empty v3 queue. Called before the region is
 * published to the peer, so plain stores are sufficient except for the
 * final tail release. */
void axivc_ring_initialize(struct axivc_ring *ring, u32 direction);

/* Returns false when the ring has no free cell; the cell is never
 * overwritten in that case. */
bool axivc_ring_try_push_cell(
	struct axivc_ring *ring, const u8 cell[AXIVC_CELL_SIZE]);

/* Copies the oldest published cell without consuming it. Returns false when
 * the ring is empty. The consumer must call axivc_ring_pop_cell() only after
 * the peeked cell has been fully validated and copied. */
bool axivc_ring_try_peek_cell(
	struct axivc_ring *ring, u8 cell[AXIVC_CELL_SIZE]);

/* Consumes one previously peeked cell. Must only be called after a
 * successful axivc_ring_try_peek_cell(). */
void axivc_ring_pop_cell(struct axivc_ring *ring);
