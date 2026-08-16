#pragma once

#include <linux/types.h>

#include <ivc/ioctl_args.h>
#include <ivc/ivc_dev.h>

#define MAX_VDEVS 16

int init_ivc_devices(void);
void uninit_ivc_devices(void);
