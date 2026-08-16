# Makefile for building the kernel module

CROSS_COMPILE ?= aarch64-linux-gnu-
ARCH ?= arm64

# Kernel build directory
KDIR ?= /lib/modules/$(shell uname -r)/build

# Error messages from gcc will be muted if we omit an explicit definition to CC.
CC := ${CROSS_COMPILE}gcc

# Current path
CURRENT_PATH := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

# Extra compiler flags
EXTRA_CFLAGS ?=
ccflags-y    += $(EXTRA_CFLAGS) -I $(CURRENT_PATH)/../include/


# Module name
MODULE := axvisor-driver

# Source files
SRC := main.o \
	   hvc.o \
	   ring.o \
	   message.o \
	   region.o \
	   ivc.o

# Default target
all:
	CROSS_COMPILE=$(CROSS_COMPILE) ARCH=$(ARCH) $(MAKE) -C $(KDIR) M=$(CURRENT_PATH) modules

# Clean target
clean:
	CROSS_COMPILE=$(CROSS_COMPILE) ARCH=$(ARCH) $(MAKE) -C $(KDIR) M=$(CURRENT_PATH) clean

# Install target
install:
	@sudo cp $(MODULE).ko /lib/modules/$(shell uname -r)/kernel/drivers/
	@sudo depmod -a

# Uninstall target
uninstall:
	@sudo rm -f /lib/modules/$(shell uname -r)/kernel/drivers/$(MODULE).ko
	@sudo depmod -a

# Module build rules
obj-m := axvisor.o
axvisor-objs := $(SRC)
