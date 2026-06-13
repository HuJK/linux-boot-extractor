#!/usr/bin/env bash
# End-to-end lbx test matrix: for each arch, extract kernel+initrd+cmdline
# from a set of images and boot them in QEMU (../scripts/verify-boot.sh),
# then print a summary.
#
# Self-contained — it bootstraps what it needs:
#   - a local alpine disk per arch (tests/alpine_<arch>.qcow2), downloaded if
#     missing; a self-contained image that boots to userspace (root mount),
#     so it exercises the *whole* pipeline, not just kernel+initrd;
#   - an image-URL list per arch (tests/image_urls_<arch>.txt), generated via
#     fetch-image-urls.py if missing; each URL is analysed straight from the
#     cloud (lazy range fetch) and booted kernel+initrd -> initramfs shell.
#
# Usage:
#   tests/run-boot-tests.sh                # both arches, every image, serial
#   tests/run-boot-tests.sh -j 4           # up to 4 boots in parallel
#   LBX_TEST_ARCHES=amd64 tests/run-boot-tests.sh
#   LBX_TEST_TIMEOUT=200 LBX_TEST_CACHE=~/.cache/lbx tests/run-boot-tests.sh
#
# Needs: a built lbx (or cargo to build one), qemu-system-{aarch64,x86_64},
# python3, and curl or wget. arm64 guests run under TCG (slow) on an x86
# host; amd64 guests use KVM when available (seconds).
set -uo pipefail
cd "$(dirname "$0")/.."                                  # repo root

TESTS=tests
VERIFY=scripts/verify-boot.sh
PY=$TESTS/fetch-image-urls.py
# Repo-local + gitignored, so downloaded chunks persist and reuse across runs.
CACHE="${LBX_TEST_CACHE:-tests/.boot-cache}"
TIMEOUT="${LBX_TEST_TIMEOUT:-150}"
ARCHES="${LBX_TEST_ARCHES:-arm64 amd64}"
JOBS="${LBX_TEST_JOBS:-1}"   # boots to run concurrently (-j N)
# Boot every image's kernel+initrd into the per-arch alpine baseline as root
# (uniform: each reaches the same Alpine login). 0 = each image's own root /
# an initramfs shell instead.
SHARED_ROOT="${LBX_TEST_SHARED_ROOT:-1}"

while [ $# -gt 0 ]; do
    case "$1" in
        -j)  JOBS="${2:?-j needs a number}"; shift 2 ;;
        -j*) JOBS="${1#-j}"; shift ;;
        -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1 (try -j N)" >&2; exit 2 ;;
    esac
done

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# --- lbx binary: an existing build, or build one with cargo ---
LBX="${LBX:-}"
if [ -z "$LBX" ]; then
    for c in target/release/lbx target/debug/lbx dist/lbx-linux-x64 "$(command -v lbx || true)"; do
        [ -n "$c" ] && [ -x "$c" ] && LBX="$c" && break
    done
fi
if [ -z "$LBX" ]; then
    command -v cargo >/dev/null || die "no lbx binary and no cargo to build one"
    log "building lbx (cargo build --release)"
    cargo build --release >&2 || die "cargo build failed"
    LBX=target/release/lbx
fi
export LBX
log "lbx: $LBX"

download() {
    local url="$1" out="$2"
    if command -v curl >/dev/null; then curl -fL --retry 3 -o "$out" "$url"
    elif command -v wget >/dev/null; then wget -qO "$out" "$url"
    else die "need curl or wget to download $out"; fi
}

RESULTS=$(mktemp); RDIR=$(mktemp -d); trap 'rm -rf "$RESULTS" "$RDIR"' EXIT

# run_one <arch> <label> <locator> <result-file>  — extract + boot one image,
# printing a live row and writing its TSV result (safe to run in parallel:
# each job owns its result file, and lbx keys its cache on the URL).
run_one() {
    local arch="$1" label="$2" loc="$3" rf="$4"
    local comp t0 t1 out boot detail
    # Each (backgrounded) job runs in its own subshell, so exporting the
    # shared root here is isolated per arch.
    local rootdisk="$TESTS/alpine_$arch.qcow2"
    [ "$SHARED_ROOT" = 1 ] && [ -f "$rootdisk" ] && export LBX_VERIFY_ROOT_DISK="$rootdisk"
    comp=$(timeout "$TIMEOUT" "$LBX" boot-info "$loc" --cache-dir "$CACHE/$arch" 2>/dev/null \
            | awk '/^compression:/{print $2; got=1} END{if(!got) print "raw"}')
    t0=$(date +%s)
    out=$(LBX_VERIFY_TIMEOUT="$TIMEOUT" timeout $((TIMEOUT + 90)) \
            bash "$VERIFY" "$loc" --cache-dir "$CACHE/$arch" 2>&1)
    t1=$(date +%s)
    if grep -q '^PASS' <<<"$out"; then
        boot=PASS; detail=$(grep -oE 'reached-userspace=[a-z]+' <<<"$out" | head -1)
    elif grep -q '^FAIL:' <<<"$out"; then
        boot=BOOT-FAIL; detail=$(grep -oiE 'FAIL:.*' <<<"$out" | head -1 | cut -c1-46)
    else
        boot=EXTRACT-FAIL
        detail=$(grep -oiE 'no boot entries[^"]*|unrecognized filesystem[^"]*|no readable filesystem[^"]*|no boot artifacts[^"]*' \
                    <<<"$out" | head -1 | cut -c1-46)
        [ -z "$detail" ] && detail=$(grep -i 'error\|caused by' <<<"$out" | tail -1 | cut -c1-46)
    fi
    printf '  %-13s %-6s %-12s %-12s %4ss  %s\n' "$label" "$arch" "$comp" "$boot" "$((t1 - t0))" "$detail"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$arch" "$comp" "$boot" "$((t1 - t0))" "$detail" >"$rf"
}

# Bootstrap each arch (URL list + local alpine baseline) sequentially, then
# collect every (arch, image) into one job list.
JOBLIST=()
for arch in $ARCHES; do
    list="$TESTS/image_urls_$arch.txt"
    if [ ! -f "$list" ]; then
        log "generating $list"
        python3 "$PY" --arch "$arch" --variant default >"$list" 2>/dev/null \
            || warn "fetch-image-urls.py failed for $arch"
    fi
    base="$TESTS/alpine_$arch.qcow2"
    if [ ! -f "$base" ]; then
        aurl=$(grep -m1 '/alpine/' "$list" 2>/dev/null || true)
        if [ -n "$aurl" ]; then log "downloading $base"; download "$aurl" "$base" || warn "alpine $arch download failed"
        else warn "no alpine URL in $list for the $arch baseline"; fi
    fi
    [ -f "$base" ] && JOBLIST+=("$arch"$'\t'"alpine[local]"$'\t'"$base")
    while read -r url; do
        [ -z "$url" ] && continue
        case "$url" in \#*) continue ;; esac
        JOBLIST+=("$arch"$'\t'"$(sed -E 's#.*/images/([^/]+)/.*#\1#' <<<"$url")"$'\t'"$url")
    done <"$list"
done

log "running ${#JOBLIST[@]} boots, up to $JOBS at a time"
printf '  %-13s %-6s %-12s %-12s %5s  %s\n' IMAGE ARCH COMPRESSION RESULT TIME DETAIL
i=0
for job in "${JOBLIST[@]}"; do
    IFS=$'\t' read -r arch label loc <<<"$job"
    run_one "$arch" "$label" "$loc" "$RDIR/$i" &
    i=$((i + 1))
    while [ "$(jobs -rp | wc -l)" -ge "$JOBS" ]; do wait -n; done
done
wait
cat "$RDIR"/* >"$RESULTS" 2>/dev/null || true

# --- summary ---
echo
log "Summary  ($(grep -c PASS "$RESULTS") PASS / $(grep -cE 'BOOT-FAIL|EXTRACT-FAIL' "$RESULTS") fail of $(wc -l <"$RESULTS") total)"
printf '%-13s %-6s %-12s %-12s %5s  %s\n' IMAGE ARCH COMPRESSION RESULT TIME DETAIL
sort -k2,2 -k1,1 "$RESULTS" | while IFS=$'\t' read -r label arch comp boot time detail; do
    printf '%-13s %-6s %-12s %-12s %4ss  %s\n' "$label" "$arch" "$comp" "$boot" "$time" "$detail"
done
# Non-zero exit if any boot (as opposed to a known-unsupported extract) failed.
! grep -q 'BOOT-FAIL' "$RESULTS"
