#pragma once

enum hvc_fid
{
	HIVCPublishChannel = 3,
	HIVCSubscribChannel = 4,
	HIVCUnPublishChannel = 5,
	HIVCUnSubscribChannel = 6,
	HIVCNotify = 7,
};

u64 hvc_call(
	u64 hvc_mode, u64 arg0, u64 arg1, u64 arg2, u64 arg3, u64 arg4,
	u64 arg5) noinline;

u64 hvc_publish_channel(u64 channel_key, u64 shm_base_ptr, u64 shm_size_ptr);
u64 hvc_unpublish_channel(u64 channel_key);
u64 hvc_subscribe_channel(
	u64 publisher_id, u64 channel_key, u64 shm_base_ptr, u64 shm_size_ptr);
u64 hvc_unsubscribe_channel(u64 publisher_id, u64 channel_key);
u64 hvc_notify_channel(u64 publisher_id, u64 channel_key, u64 target_vm_id);
