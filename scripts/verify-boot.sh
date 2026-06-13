#!/usr/bin/env bash
# End-to-end correctness check for an lbx extraction: pull kernel + initrd +
# cmdline out of an image (local path or http(s) URL), then boot them in QEMU
# the same way a VMM does — direct kernel boot (-kernel/-initrd), no firmware
# — and confirm the kernel decompresses, the initrd unpacks, and (when a disk
# + cmdline are available) userspace is reached. This proves the extracted
# files actually boot, not merely that they're well-formed.
#
#   scripts/verify-boot.sh <image|url> [extra lbx-extract args...]
#   LBX_VERIFY_TIMEOUT=120 scripts/verify-boot.sh tests/ubuntu.qcow2
#
# Needs: a built lbx (target/{release,debug}/lbx or on PATH) and
# qemu-system-aarch64 / qemu-system-x86_64. Exit 0 = PASS.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE="${1:?usage: verify-boot.sh <image|url> [extra lbx-extract args...]}"; shift || true
TIMEOUT="${LBX_VERIFY_TIMEOUT:-90}"

LBX="${LBX:-}"
[ -n "$LBX" ] || for c in target/release/lbx target/debug/lbx dist/lbx-linux-x64 "$(command -v lbx || true)"; do
    [ -n "$c" ] && [ -x "$c" ] && LBX="$c" && break
done
[ -n "$LBX" ] || { echo "error: no lbx binary found (build it first)" >&2; exit 2; }

is_url=false; case "$IMAGE" in http://*|https://*) is_url=true ;; esac

OUT=$(mktemp -d); trap 'rm -rf "$OUT"' EXIT
echo "==> lbx extract $IMAGE"
# Capture the copy log instead of guessing kernel filenames: extract prints
# "  -> <path>" for each file it writes, kernel first, then initrd(s), then
# configs, then cmdline. This copes with any naming (vmlinux, <hash>-Image.efi).
if ! out=$("$LBX" extract "$IMAGE" -o "$OUT" --decompress --vdafix "$@" 2>&1); then
    echo "$out"; echo "FAIL: extract failed" >&2; exit 1
fi
echo "$out"
mapfile -t copied < <(printf '%s\n' "$out" | sed -n 's/^[[:space:]]*-> //p')
KERNEL="${copied[0]:-}"
INITRD=""
for f in "${copied[@]:1}"; do
    case "$(basename "$f")" in *init*) INITRD="$f"; break ;; esac
done
CMDLINE=$(cat "$OUT/cmdline" 2>/dev/null || true)
[ -n "$KERNEL" ] || { echo "FAIL: no kernel extracted" >&2; exit 1; }

# Architecture from the (now decompressed) kernel: an arm64 Image carries the
# magic "ARMd" at offset 0x38; anything else is treated as x86 bzImage.
if [ "$(dd if="$KERNEL" bs=1 skip=56 count=4 2>/dev/null)" = "ARMd" ]; then
    QEMU=qemu-system-aarch64
    # cortex-a72 (ARMv8.0) dodges a TCG assert that -cpu max hits on some
    # very new kernels, and still boots everything we target.
    MACHINE=(-machine virt -cpu cortex-a72)
    CONSOLE=ttyAMA0
else
    QEMU=qemu-system-x86_64
    CONSOLE=ttyS0
    # KVM accelerates a same-arch (x86 guest on x86 host) boot to near
    # native; otherwise fall back to TCG.
    if [ -e /dev/kvm ] && [ "$(uname -m)" = "x86_64" ]; then
        MACHINE=(-enable-kvm -cpu host)
    else
        MACHINE=(-cpu max)
    fi
fi
command -v "$QEMU" >/dev/null || { echo "error: $QEMU not installed" >&2; exit 2; }

# Pick what root the kernel+initrd boot into:
#  1. LBX_VERIFY_ROOT_DISK set → boot every image into one shared root disk
#     (root=/dev/vdaN), so behaviour is uniform: the extracted kernel+initrd
#     mount that disk and reach its login, whatever distro they came from.
#  2. else a local image with a real cmdline → boot its own root.
#  3. else (URL, or no cmdline) → an initramfs shell, proving the two files.
DISK=()
if [ -n "${LBX_VERIFY_ROOT_DISK:-}" ]; then
    APPEND="root=${LBX_VERIFY_ROOT_DEV:-/dev/vda2} rw rootfstype=ext4 rootwait console=$CONSOLE loglevel=7"
    DISK=(-drive "if=virtio,file=$LBX_VERIFY_ROOT_DISK,format=qcow2,snapshot=on")
elif ! $is_url && [ -n "$CMDLINE" ]; then
    APPEND="$CMDLINE console=$CONSOLE loglevel=7"
    DISK=(-drive "if=virtio,file=$IMAGE,format=qcow2,snapshot=on")
else
    APPEND="console=$CONSOLE loglevel=7 rdinit=/bin/sh"
fi

LOG="$OUT/serial.log"
echo "==> $QEMU (${CONSOLE}), ${TIMEOUT}s cap"
timeout "$TIMEOUT" "$QEMU" "${MACHINE[@]}" -m 2048 -smp 2 \
    -kernel "$KERNEL" ${INITRD:+-initrd "$INITRD"} \
    -append "$APPEND" "${DISK[@]}" \
    -display none -serial "file:$LOG" -no-reboot 2>"$OUT/qemu.err" || true

fail() {
    echo "FAIL: $1" >&2
    [ -s "$OUT/qemu.err" ] && { echo "--- qemu stderr ---" >&2; tail -3 "$OUT/qemu.err" >&2; }
    echo "--- last serial ---" >&2; tail -8 "$LOG" 2>/dev/null >&2
    exit 1
}

grep -aq "Linux version" "$LOG" || fail "kernel never started (bad Image / wrong magic)"
grep -aqi "Initramfs unpacking failed" "$LOG" && fail "initrd corrupt: 'Initramfs unpacking failed'"
if [ -n "$INITRD" ]; then
    grep -aqiE "Freeing initrd memory|Unpacking initramfs" "$LOG" \
        || fail "initrd never unpacked (kernel didn't reach it)"
fi

ver=$(grep -aoE "Linux version [^ ]+" "$LOG" | head -1)
userspace="no"
grep -aqiE "systemd\[1\]|switch_root|sh-[0-9.]+#|Run /init|Reached target|OpenRC|Welcome to Alpine| login:|Entering runlevel" \
    "$LOG" && userspace="yes"
echo "PASS: $ver booted; ${INITRD:+initrd unpacked; }reached-userspace=$userspace"
