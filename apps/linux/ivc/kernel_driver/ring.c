#include <asm/barrier.h>
#include <linux/build_bug.h>
#include <linux/compiler.h>
#include <linux/string.h>
#include <linux/types.h>

#include "includes/ring.h"

static void axivc_ring_check_layout(void)
{
	BUILD_BUG_ON(sizeof(struct axivc_ring) != 1088);
	BUILD_BUG_ON(offsetof(struct axivc_ring, cells) != 64);
}

void axivc_ring_initialize(struct axivc_ring *ring, u32 direction)
{
	axivc_ring_check_layout();
	WRITE_ONCE(ring->direction, direction);
	WRITE_ONCE(ring->capacity, AXIVC_RING_CAPACITY);
	WRITE_ONCE(ring->cell_size, AXIVC_CELL_SIZE);
	WRITE_ONCE(ring->head, 0);
	memset(ring->cells, 0, sizeof(ring->cells));
	/* Publishing tail last lets a peer that observes an empty ring trust
	 * the layout fields above. */
	smp_store_release(&ring->tail, 0);
}

bool axivc_ring_try_push_cell(
	struct axivc_ring *ring, const u8 cell[AXIVC_CELL_SIZE])
{
	u32 tail = READ_ONCE(ring->tail);
	u32 head = smp_load_acquire(&ring->head);
	u32 cell_index;

	if ((u32)(tail - head) >= AXIVC_RING_CAPACITY)
		return false;

	cell_index = tail % AXIVC_RING_CAPACITY;
	memcpy(ring->cells[cell_index], cell, AXIVC_CELL_SIZE);
	smp_store_release(&ring->tail, tail + 1);
	return true;
}

bool axivc_ring_try_peek_cell(struct axivc_ring *ring, u8 cell[AXIVC_CELL_SIZE])
{
	u32 head = READ_ONCE(ring->head);
	u32 tail = smp_load_acquire(&ring->tail);
	u32 cell_index;

	if (head == tail)
		return false;

	cell_index = head % AXIVC_RING_CAPACITY;
	memcpy(cell, ring->cells[cell_index], AXIVC_CELL_SIZE);
	return true;
}

void axivc_ring_pop_cell(struct axivc_ring *ring)
{
	u32 head = READ_ONCE(ring->head);

	smp_store_release(&ring->head, head + 1);
}
