# linux-boot-extractor (`lbx`)

Pure-userspace, read-only extraction of `vmlinuz` / `initramfs` / boot
configs from VM disk images — no mounting, no root, no qemu appliance.

Motivation: crosvm on gunyah has no usable UEFI path, so guests must be
direct-kernel-booted (`vmlinuz` + `initramfs` + cmdline). This tool/library
pulls those straight out of a `.qcow2`/raw image so a VM manager can accept
a normal distro disk image.

## Usage

The image may be a local path **or an `http(s)://` URL** — a remote image is
analysed lazily, downloading only the byte ranges actually read (see [Cloud
analysis](#cloud-analysis-remote-images)). Files inside it are addressed as
URIs: `p2:/boot/vmlinuz` means partition 2; a bare path searches every
readable partition.

```console
$ lbx boot-info disk.qcow2          # what the bootloader would boot:
kernel:  p2:/vmlinuz-6.6.9          #   kernel/initrd URIs + cmdline
initrd:  p2:/initramfs-6.6.9.img
cmdline: root=UUID=... console=hvc0
source:  grub
$ lbx boot-info disk.qcow2 --json   # same, machine-readable (for the VMM)

$ lbx entries disk.qcow2 [--json]   # ALL boot entries, '*' marks default
$ lbx info    disk.qcow2            # image format + partition table + fs types

$ lbx ls  disk.qcow2 p2:/boot       # list files
$ lbx cat disk.qcow2 p2:/boot/grub/grub.cfg   # file to stdout
$ lbx cp  disk.qcow2 p2:/vmlinuz-6.6.9 ./     # file to disk

$ lbx extract disk.qcow2 -o outdir  # copy default entry's kernel/initrd/
$ lbx extract disk.qcow2 --entry 2  #   configs/cmdline in one go
$ lbx extract disk.qcow2 --decompress   # unwrap a compressed kernel to a
                                    #   raw, direct-bootable Image (below)
$ lbx cp  disk.qcow2 p2:/boot/vmlinuz-6.6.9 ./ --decompress   # same, one file
$ lbx md5 disk.qcow2 p2:/boot/vmlinuz-6.6.9 --decompress       # md5 of that

# Any command also takes a URL; --cache-dir reuses downloaded chunks:
$ lbx boot-info https://cloud-images.example/img.qcow2 --cache-dir ~/.cache/lbx

$ lbx shell disk.qcow2              # interactive debugging:
lbx p2:/> ls boot                   #   parts/use/cd/pwd/ls/cat/cp/boot
lbx p2:/> cp p1:/EFI/BOOT/grub.cfg /tmp/
```

## Architecture

One trait, `blockdev::ReadAt` (byte-addressed, read-only, `&self`), stacks
every layer:

```
byte source (file / URL)          source::open()     -> impl ReadAt
  └─ disk image (qcow2 / raw)      disk::open()       -> impl ReadAt
       └─ partition table (GPT/MBR) part::scan()      -> Vec<Partition>
            └─ partition slice     blockdev::Slice    -> impl ReadAt
                 └─ filesystem     fsys::open()       -> Box<dyn FileSystem>
                      └─ boot files boot::scan()      -> BootEntry, configs
```

The crate is a library first (`lbx`); the CLI binary is a thin wrapper.
The intended end state is the VMM linking the library and loading the
kernel/initrd directly into guest memory, no temp files.

| Layer | Implementation | Status |
|---|---|---|
| byte source | own code (`source/`) | local file (`pread`); http(s) URL with lazy 4 MiB chunks, `--cache-dir`, redirects, pure-Rust TLS (rustls + RustCrypto, embedded roots, warn-only) |
| qcow2 | own code (`disk/qcow2.rs`) | v2/v3 read: sparse/zero clusters, zlib-compressed clusters, backing file chains; **no** zstd / encryption / extended L2 |
| raw | trivial | done |
| GPT / MBR (+EBR chain) | own code (`part/`) | done |
| LVM2 | — | planned; /boot is almost always a plain partition, so low priority |
| ext2/3/4 | [`ext4-view`](https://crates.io/crates/ext4-view) | done |
| FAT12/16/32 (ESP) | [`fatfs`](https://crates.io/crates/fatfs) | done |
| XFS | — | planned (RHEL 9 `/boot` default); detected with a clear error today |
| btrfs / squashfs | — | detected with a clear error |
| grub.cfg parser | `boot/grub.rs` | done: menuentry/submenu, linux*/initrd* variants, `set default` + grubenv `saved_entry` (index, `N>M`, id, title), `blscfg` redirect |
| BLS (`/loader/entries`) | `boot/bls.rs` | done: version sort, `$kernelopts`/`$tuned_params` from grubenv, loader.conf `default` glob, boot-counting suffixes |
| extlinux/syslinux | `boot/extlinux.rs` | done: `DEFAULT` label, `initrd=` lifted out of `APPEND` |
| grubenv | `boot/grubenv.rs` | done |
| fallback vmlinuz scan | `boot/mod.rs` | done: rpm-style version sort (`boot/vercmp.rs`), rescue/kdump excluded, `/vmlinuz` symlink honored |

## Filesystem support rationale

We only need to read `/boot` and the ESP, not the root filesystem:

- **vfat** — every UEFI install has an ESP; systemd-boot/UKI setups keep the
  kernel there. Required.
- **ext4** — default `/boot` on Ubuntu, Debian, Fedora, Arch. Required.
- **XFS** — RHEL/Rocky/Alma 9 use xfs for `/boot`. Next milestone.
- **btrfs** — openSUSE keeps `/boot` in a btrfs subvolume. Later.
- **LVM/LUKS** — `/boot` is essentially never on LVM, and encrypted images
  can't be read without keys. Out of scope for now.

## Multi-kernel images

`boot::scan` returns *all* entries plus which one the bootloader would pick
(`BootScan::default`), resolved per source: GRUB `set default`/grubenv
`saved_entry` → BLS version sort (+ saved_entry / loader.conf pin) →
extlinux `DEFAULT` → fallback version sort with `/vmlinuz` symlink hint.
Entries whose kernel file no longer exists are dropped. `lbx entries` lists
everything; `lbx extract --entry N` overrides the default.

Extracted cmdlines are written to `<outdir>/cmdline`. `root=UUID=...`
works as-is under virtio; `root=/dev/sdX2`-style device names are handled
by **vdafix** (`vdafix::fix_cmdline`): the partition number is looked up
in the image's own partition table and the value is rewritten to
`root=PARTUUID=...` (GPT unique GUID, or MBR `<disk-signature>-NN`),
which the kernel resolves even without an initramfs. JSON output carries
the rewrite as `cmdline_fixed` (null when nothing applies); `extract
--vdafix` writes the fixed cmdline. Whole-disk `root=/dev/sda` and
multi-disk installs can't be mapped and are left untouched.

## Compressed kernels (direct-kernel boot)

A direct-kernel-boot VMM (crosvm, `qemu -kernel`) loads the kernel itself,
with no bootloader in front of it, and on arm64 it needs the raw `Image`
(64-byte header, magic `ARMd` at offset 0x38). But distros don't ship that
as `vmlinuz` — normally GRUB or the EFI stub decompresses the kernel at boot,
so the on-disk `vmlinuz` is wrapped, and a VMM handed it straight rejects it
("invalid magic number"). Two wrappers are handled:

- **gzip** of the `Image` (`Image.gz`) — Debian/Ubuntu arm64;
- **EFI zboot**: a small PE shell (`zimg` marker at offset 4) carrying a
  gzip/zstd-compressed `Image` as its payload — Fedora arm64;
- a **UKI** (systemd-stub Unified Kernel Image): a PE/EFI executable bundling
  kernel + initrd + cmdline + DTBs, with the real kernel in a `.linux` PE
  section (itself usually gzip/zboot) — newer Ubuntu arm64.

Unwrapping recurses, so a UKI whose `.linux` is `Image.gz` still comes out a
raw `Image`. x86 `vmlinuz` is a self-extracting `bzImage` the VMM already
groks, and some arm64 images ship the raw `Image`; both are left untouched.

The tool stays faithful by default — `cp`/`extract`/`md5` copy/hash the
bytes as stored. Decompression is opt-in:

- `entries`/`boot-info` report the wrapper per entry as
  `kernel_compression` (JSON), e.g. `"gzip"` or `"zboot+zstd"`; `null` means
  the kernel is already raw and needs no unwrapping. The embedding VMM reads
  this to decide.
- `cp --decompress` / `extract --decompress` write the unwrapped raw
  `Image`; `md5 --decompress` digests it, so the hash matches the extracted
  file (used by a caching VMM to validate the cache). On an already-raw
  kernel the flag is a no-op; on a recognized-but-unsupported zboot codec it
  errors rather than silently emitting an unbootable kernel.

## Cloud analysis (remote images)

The image locator can be an `http(s)://` URL, so a distro's published cloud
image is analysed in place — no full download. The whole tool reads through
one `ReadAt` seam ([`source`](src/source/)), so qcow2/partition/fs parsing
doesn't care whether the bytes come from a file or the network. A remote
source:

- **probes once** for the total size and whether the server honours
  `Range` (a `bytes=0-0` request: `206`+`Content-Range` ⇒ yes, `200` ⇒ no);
- serves reads from a cache of fixed **4 MiB chunks**, fetching a chunk the
  first time it's touched and blocking until it lands:
  - range supported → fetch only the touched chunks (random access);
  - range *not* supported → stream forward from byte 0, caching as it goes,
    until the wanted chunk is reached (no seeking, so the bytes in front of
    it download too).

So `boot-info`/`extract` on a multi-hundred-MB cloud qcow2 pulls only a few
dozen MB — the metadata clusters plus the kernel/initrd. The tool never
writes the source.

`--cache-dir <dir>` persists chunks at `<dir>/<md5(url)>/<chunk-index>` so a
later run (a second `extract`, or `boot-info` then `extract`) reuses them
and re-downloads nothing. Redirects (301/302/303/307/308) and
`Transfer-Encoding: chunked` bodies are handled; the size probe needs the
server to send `Content-Length`/`Content-Range` (cloud storage and CDNs do).

**TLS is pure Rust** (rustls + the RustCrypto provider), so the static
musl/Android binary needs no C toolchain. Server certificates are checked
against the Mozilla roots embedded via `webpki-roots` (the device's own
trust store, which varies by vendor, is not used); a verification failure
**warns and proceeds** — the transport is encrypted but unauthenticated, on
the assumption that image integrity comes from a separate checksum.

## Roadmap

1. ~~Skeleton: qcow2 → GPT/MBR → ext4/vfat → CLI~~ (done)
2. ~~Boot config parsers (grub.cfg/BLS/extlinux) + default-entry selection~~ (done)
3. ~~qcow2: backing files, zlib-compressed clusters~~ (done; zstd still open)
4. XFS read-only.
5. Library API polish for VMM embedding (read kernel/initrd to memory).

Verified against real images: Debian 13 trixie arm64 (GRUB entries incl.
submenu) and CentOS Stream 10 (zlib-compressed qcow2, BLS entries) — full
extract from the compressed image takes ~0.6 s.

## Testing

`scripts/make-test-image.sh [dir]` builds a GPT raw+qcow2 test image
(needs sfdisk, mkfs.vfat, mcopy, mke2fs, qemu-img), then:

```console
$ cargo run -- extract /tmp/lbxtest/disk.qcow2 -o /tmp/lbxtest/out
```

### Boot verification

`scripts/verify-boot.sh <image|url>` is the end-to-end correctness check: it
extracts the kernel/initrd/cmdline and boots them in QEMU exactly as a VMM
would (`-kernel`/`-initrd`, no firmware), then asserts the kernel
decompressed, the initrd unpacked, and — with a local disk + cmdline —
userspace was reached. It proves the extracted files actually boot, not just
that they parse.

```console
$ scripts/verify-boot.sh tests/alpine_arm64.qcow2          # local, full boot
$ scripts/verify-boot.sh https://.../disk.qcow2            # cloud, lazy fetch
```

For a URL (or an image whose cmdline came from BLS/grubenv) there's no local
rootfs, so it boots kernel+initrd to an initramfs shell (`rdinit=/bin/sh`) —
still a full proof of the two files. Arch is taken from the kernel (`ARMd`
magic → `qemu-system-aarch64`, else x86 `bzImage` → `qemu-system-x86_64`,
with `-enable-kvm` when the host can). Note arm64 guests on an x86 host run
under TCG (slow); x86 guests with KVM boot in seconds — but an x86 `vmlinuz`
is a `bzImage` that needs no decompression, so only arm64 exercises the
gzip/zboot/UKI unwrap.
