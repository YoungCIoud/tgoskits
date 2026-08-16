# TEMPORARY(quick-iteration) prebuilt IVC artifacts

These files let the qemu-ivc CI job patch a local rootfs without requiring
the guest Linux kernel source tree on the runner.

- `axvisor.ko`: built from `apps/linux/ivc/kernel_driver` against guest
  kernel `7.1.0-rc2-g74fe02ce122a-dirty`.
- `ivc-publish` / `ivc-subscribe`: built by `apps/linux/ivc/build.sh`.

Remove this directory and the CI pre-step once tgosimages publishes a rootfs
with matching Message V1 artifacts.

Regenerate locally with:
  scripts/ivc-local-e2e.sh --no-run --update-prebuilt
