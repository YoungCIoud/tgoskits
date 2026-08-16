#include <linux/types.h>

#include "includes/hvc.h"

/// Refer to
/// https://github.com/arceos-hypervisor/arm_vcpu/blob/1e107bc68bedb82c387eedb423d08c84ff9ea09e/src/exception.rs#L97
u64 hvc_call(
	u64 hvc_mode, u64 arg0, u64 arg1, u64 arg2, u64 arg3, u64 arg4, u64 arg5)
{
	register u64 reg_x0 asm("x0") = hvc_mode;
	register u64 reg_x1 asm("x1") = arg0;
	register u64 reg_x2 asm("x2") = arg1;
	register u64 reg_x3 asm("x3") = arg2;
	register u64 reg_x4 asm("x4") = arg3;
	register u64 reg_x5 asm("x5") = arg4;
	register u64 reg_x6 asm("x6") = arg5;

	asm volatile("hvc #0\n"
				 "nop"
				 : "+r"(reg_x0)
				 : "r"(reg_x1), "r"(reg_x2), "r"(reg_x3), "r"(reg_x4),
				   "r"(reg_x5), "r"(reg_x6)
				 : "memory");

	return reg_x0;
}

u64 hvc_publish_channel(u64 channel_key, u64 shm_base_ptr, u64 shm_size_ptr)
{
	return hvc_call(
		HIVCPublishChannel, channel_key, shm_base_ptr, shm_size_ptr, 0, 0, 0);
}

u64 hvc_unpublish_channel(u64 channel_key)
{
	return hvc_call(HIVCUnPublishChannel, channel_key, 0, 0, 0, 0, 0);
}

u64 hvc_subscribe_channel(
	u64 publisher_id, u64 channel_key, u64 shm_base_ptr, u64 shm_size_ptr)
{
	return hvc_call(
		HIVCSubscribChannel, publisher_id, channel_key, shm_base_ptr,
		shm_size_ptr, 0, 0);
}
u64 hvc_unsubscribe_channel(u64 publisher_id, u64 channel_key)
{
	return hvc_call(
		HIVCUnSubscribChannel, publisher_id, channel_key, 0, 0, 0, 0);
}

u64 hvc_notify_channel(u64 publisher_id, u64 channel_key, u64 target_vm_id)
{
	return hvc_call(HIVCNotify, publisher_id, channel_key, target_vm_id, 0, 0, 0);
}
