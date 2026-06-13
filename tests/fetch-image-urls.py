#!/usr/bin/env python3
"""Print the latest <arch>/<variant> `disk.qcow2` URL for every distribution
on https://images.linuxcontainers.org/images/.

The site is a plain nginx autoindex. The path layout is:

    images/<distro>/<release>/<arch>/<variant>/<timestamp>/disk.qcow2

arch/variant default to arm64/cloud and are overridable with --arch /
--variant (e.g. `--arch amd64 --variant default`).

At each "pick the latest" step we go by the folder's modification date shown
in the listing (newest wins; ties broken by the larger name, which is what
makes Debian's stable `trixie` beat the same-dated `forky`). Distros that
have no such arch/variant build, or whose newest build ships no `disk.qcow2`
(rootfs-only), are reported on stderr and skipped.

stdout is one URL per line, so it pipes cleanly into wget/xargs.
"""

import argparse
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime

BASE = "https://images.linuxcontainers.org/images/"

# An autoindex row for a subdirectory, e.g.:
#   <a href="trixie/">trixie/</a>            27-Sep-2025 07:32       -
# The href is left percent-encoded (timestamp dirs contain ':' -> %3A) so it
# can be dropped straight into a URL.
_DIR_ROW = re.compile(
    r'<a href="(?P<name>[^"?][^"]*)/">[^<]*</a>\s+'
    r'(?P<date>\d{2}-[A-Za-z]{3}-\d{4} \d{2}:\d{2})'
)


def _fetch(url):
    with urllib.request.urlopen(url, timeout=30) as resp:
        return resp.read().decode("utf-8", "replace")


def subdirs(url):
    """[(name, mtime)] of subdirectories at `url`, newest-first."""
    rows = []
    for m in _DIR_ROW.finditer(_fetch(url)):
        name = m.group("name")
        if name == "..":
            continue
        mtime = datetime.strptime(m.group("date"), "%d-%b-%Y %H:%M")
        rows.append((name, mtime))
    rows.sort(key=lambda r: (r[1], r[0]), reverse=True)
    return rows


def latest_subdir(url):
    rows = subdirs(url)
    return rows[0][0] if rows else None


def has_file(url, name):
    return f'<a href="{name}">' in _fetch(url)


def main(arch, variant):
    for distro, _ in subdirs(BASE):
        try:
            release = latest_subdir(BASE + distro + "/")
            if release is None:
                continue
            variant_url = f"{BASE}{distro}/{release}/{arch}/{variant}/"
            build = latest_subdir(variant_url)
        except urllib.error.HTTPError:
            # no such arch and/or variant for this distro
            print(f"# {distro}: no {arch}/{variant} build", file=sys.stderr)
            continue
        if build is None:
            print(f"# {distro} ({release}): empty {arch}/{variant}",
                  file=sys.stderr)
            continue
        build_url = f"{variant_url}{build}/"
        if not has_file(build_url, "disk.qcow2"):
            print(f"# {distro} ({release}): newest build has no disk.qcow2",
                  file=sys.stderr)
            continue
        print(build_url + "disk.qcow2")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--arch", default="arm64",
                    help="architecture directory (default: arm64)")
    ap.add_argument("--variant", default="cloud",
                    help="variant directory: cloud/default/desktop/... "
                         "(default: cloud)")
    args = ap.parse_args()
    try:
        main(args.arch, args.variant)
    except KeyboardInterrupt:
        sys.exit(130)
