# linux-boot-extractor (`lbx`)

Pure-userspace, read-only extraction of `vmlinuz` / `initramfs` / boot
configs from VM disk images — no mounting, no root, no qemu appliance.

Motivation: crosvm on gunyah has no usable UEFI path, so guests must be
direct-kernel-booted (`vmlinuz` + `initramfs` + cmdline). This tool/library
pulls those straight out of a `.qcow2`/raw image so a VM manager can accept
a normal distro disk image.

## Usage

Files are addressed as URIs: `p2:/boot/vmlinuz` means partition 2; a bare
path searches every readable partition.

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

$ lbx shell disk.qcow2              # interactive debugging:
lbx p2:/> ls boot                   #   parts/use/cd/pwd/ls/cat/cp/boot
lbx p2:/> cp p1:/EFI/BOOT/grub.cfg /tmp/
```

## Architecture

One trait, `blockdev::ReadAt` (byte-addressed, read-only, `&self`), stacks
every layer:

```
disk image (qcow2 / raw)          disk::open()       -> impl ReadAt
  └─ partition table (GPT/MBR)    part::scan()       -> Vec<Partition>
       └─ partition slice         blockdev::Slice    -> impl ReadAt
            └─ filesystem         fsys::open()       -> Box<dyn FileSystem>
                 └─ boot files    boot::scan()       -> BootEntry, configs
```

The crate is a library first (`lbx`); the CLI binary is a thin wrapper.
The intended end state is the VMM linking the library and loading the
kernel/initrd directly into guest memory, no temp files.

| Layer | Implementation | Status |
|---|---|---|
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
