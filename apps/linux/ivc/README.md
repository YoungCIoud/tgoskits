# Axvisor IVC Linux Guest Support

This directory contains the Linux-side user-space pieces used by the Axvisor
IVC QEMU test:

- `include/`: shared ioctl, user-library, and application-protocol headers.
- `lib/`: the userspace IVC wrapper and Message V1 demo payload codec.
- `publisher/`: Linux publisher program for Linux-to-ArceOS tests.
- `subscriber/`: Linux subscriber program used by the ArceOS-to-Linux test.
- `tests/`: host-side protocol and userspace-wrapper regression tests.

The vendored `kernel_driver/` snapshot builds the Linux kernel module that
exposes `/dev/axivc`. Its upstream maintenance location is
[`arceos-hypervisor/axvisor-tools`](https://github.com/arceos-hypervisor/axvisor-tools),
and `kernel_driver/VENDORED_FROM` records the exact imported revision.

## Message V1 demo protocol

Each device `read()` or `write()` transfers one complete, non-empty Message V1
logical message. POSIX zero-length reads cannot distinguish an empty message
from an empty ring, so the Linux read/write adapter rejects empty messages even
though the transport codec can represent them. The kernel module handles cell
fragmentation and reassembly; the programs in this directory define this
application payload:

```text
kind: u8 | sequence: u64 little-endian | body_len: u16 little-endian | body
```

The full-duplex demo sends five ordered Request messages with total lengths
`39, 40, 41, 640, 700`, three independently sequenced Data messages with total
lengths `41, 641, 700`, and one Ack for each Request. These lengths cover a
single cell, the fragment boundary, and messages larger than the ring's
in-flight capacity. Every receiver checks the message kind, sequence, exact
length, and deterministic body bytes.

Region v3/Message V1 intentionally rejects the older fixed-slot region v2
layout. The Linux programs and `/root/axvisor.ko` must therefore be updated
together.

## Build and test

Build the guest programs with:

```bash
AXVISOR_IVC_ARCH=aarch64 \
AXVISOR_IVC_OUT_DIR=/path/to/out \
apps/linux/ivc/build.sh
```

The output directory contains:

```text
ivc-publish
ivc-subscribe
```

The command-line forms are:

```text
ivc-publish <channel_key> [channel_size]
ivc-subscribe <publisher_vm_id> <channel_key> [request_count]
```

The Message V1 demo currently requires `request_count` to be five. Build and
run all host-side tests with:

```bash
apps/linux/ivc/build.sh --test
```

## QEMU test image

The QEMU IVC test boots its Linux guest from the tgosimages rootfs artifact.
`scripts/ivc-local-e2e.sh` builds the kernel module and user programs from the
current checkout, patches them into a local copy of that image, and then runs
the test. The kernel module must be built against the same pinned Linux release
as the guest image. Prepare a compatible external-module tree when needed:

```bash
cargo xtask image pull rootfs-aarch64-alpine.img
export IVC_E2E_KDIR="$PWD/tmp/ivc-linux-kernel"
scripts/ivc-prepare-linux-kernel.sh
scripts/ivc-local-e2e.sh
```

CI uses this same source-to-artifact path; it does not consume tracked `.ko` or
ELF files.
