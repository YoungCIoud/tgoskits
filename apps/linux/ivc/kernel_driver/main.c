/**
 * @file main.c
 * @brief A simple Linux kernel module for AxVisor hypervisor.
 */

#include <linux/module.h>

#include "includes/hvc.h"
#include "includes/ivc.h"
#include "includes/utils.h"

#define AXVISOR_VERSION "0.0.1"

/**
 * @brief Module initialization function.
 *
 * This function is called when the module is loaded into the kernel.
 * It registers the misc device and logs the initialization status.
 *
 * @return 0 on success, or a negative error code on failure.
 */
static int __init axvisor_init(void);

/**
 * @brief Module cleanup function.
 *
 * This function is called when the module is removed from the kernel.
 * It deregisters the misc device and logs the cleanup status.
 */
static void __exit axvisor_exit(void);

MODULE_AUTHOR("axvisor group");
MODULE_LICENSE("GPL v3 | Mulan PSL v2");
MODULE_DESCRIPTION("Management driver for AxVisor hypervisor");
MODULE_VERSION(AXVISOR_VERSION);

static int __init axvisor_init(void)
{
	int ret;

	INFO("Initializing AxVisor Linux kernel driver v%s\n", AXVISOR_VERSION);

	ret = init_ivc_devices();
	if (ret)
	{
		ERROR("axvisor: Failed to initialize IVC devices\n");
		return ret;
	}
	return 0;
}

static void __exit axvisor_exit(void)
{
	INFO("Exiting axvisor driver\n");
	uninit_ivc_devices();
}

module_init(axvisor_init);
module_exit(axvisor_exit);
