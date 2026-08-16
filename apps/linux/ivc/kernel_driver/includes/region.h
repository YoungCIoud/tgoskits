#pragma once

#include <linux/types.h>

#include "ring.h"

#define AXIVC_REGION_MAGIC 0x49564332U
#define AXIVC_REGION_VERSION 3U
#define AXIVC_REGION_FEATURE_SPSC_OPAQUE_CELLS 1U

/*
 * Full opaque-cell IVC region for one publisher/subscriber pair (region
 * version 3). The first two fields match axvm's host-side IVCChannelHeader:
 * the hypervisor initializes them when the host channel is created; the
 * remaining fields are owned by this shared-memory protocol.
 */
struct axivc_region_header
{
	u32 magic;
	u32 version; /* u16 protocol value stored in a u32 slot */
	u32 header_size;
	u32 region_size;
	u32 features;
	u32 publisher_to_subscriber_offset;
	u32 subscriber_to_publisher_offset;
	u32 ring_size;
} __aligned(8);

struct axivc_region
{
	u64 publisher_id;
	u64 key;
	struct axivc_region_header header;
	struct axivc_ring publisher_to_subscriber;
	struct axivc_ring subscriber_to_publisher;
} __aligned(64);

/* Resets the protocol region for a freshly published channel. The host has
 * already written publisher_id/key at channel creation; they are preserved
 * across the protocol reset and re-published once both rings are ready.
 *
 * Returns -EINVAL when the shared region cannot hold the v3 layout, or
 * -EPROTO when the host-written channel key does not match channel_key
 * (i.e. the region was not prepared by axvisor as expected). */
int axivc_region_init_publisher(void *base, size_t shm_size, u64 channel_key);

/* Validates that the region at base is a supported v3 region owned by the
 * expected publisher/channel. Returns 0 on success, -EINVAL for size or
 * publisher/key mismatch, -EAGAIN while the publisher has not finished
 * publishing the protocol header, and -EPROTO for an unsupported or
 * incompatible layout (including v2 peers). */
int axivc_region_validate(
	void *base, size_t shm_size, u64 publisher_id, u64 channel_key);
