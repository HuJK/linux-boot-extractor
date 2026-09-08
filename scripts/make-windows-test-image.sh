#!/bin/sh
# Build test images that look like Windows guests, for `lbx entries`.
# Needs: sfdisk, mkfs.vfat, mmd/mcopy, mkntfs, qemu-img, python3.
#
# Three layouts, as a real installer would leave them:
#
#   uefi.qcow2  GPT: ESP (with /EFI/Microsoft/Boot/bootmgfw.efi + BCD),
#               Microsoft Reserved, an NTFS "Windows" volume, and an NTFS
#               recovery partition. No config declares it, so `lbx entries`
#               reports the entry from detection alone (source "windows").
#   bios.qcow2  MBR: one active NTFS partition (type 0x07), Windows' MBR
#               bootstrap, and a volume boot record that chains to BOOTMGR.
#   dual.qcow2  The same GPT layout plus an ext4 /boot with a GRUB menu
#               whose os-prober `chainloader` item -- the one `set default`
#               points at -- is the Windows install.
#
# The Windows-authored bytes we can't get from Linux tools -- the PE boot
# manager, the MBR bootstrap messages, the VBR's BOOTMGR reference -- are
# synthesized here; they are exactly the bytes `guest` keys on, so the
# images exercise the detector, not a real install's every detail.
set -eu

dir="${1:-/tmp/lbxwin}"
rm -rf "$dir"
mkdir -p "$dir"
cd "$dir"

# A boot manager is a PE/COFF executable; its COFF machine field is what
# tells the VMM which firmware architecture the guest needs (0x8664 = x86-64).
python3 -c "
import struct
v = bytearray(4096)
v[0:2] = b'MZ'
pe = 0x100
v[0x3c:0x40] = struct.pack('<I', pe)
v[pe:pe+4] = b'PE\0\0'
v[pe+4:pe+6] = struct.pack('<H', 0x8664)
open('bootmgfw.efi', 'wb').write(bytes(v))
open('BCD', 'wb').write(b'regf' + bytes(4092))    # a registry hive
"

# --- UEFI / GPT ---------------------------------------------------------
truncate -s 320M uefi.raw
sfdisk --quiet uefi.raw <<'EOF'
label: gpt
unit: sectors
start=2048,   size=262144, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="EFI system partition"
start=264192, size=32768,  type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE, name="Microsoft reserved partition"
start=296960, size=131072, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="Basic data partition"
start=428032, size=131072, type=DE94BBA4-06D1-4D40-A16A-BFD50179D6AC, name="Basic data partition"
EOF

# -s 1: without it this ESP has too few clusters to be a valid FAT32.
mkfs.vfat --offset 2048 -S 512 -s 1 -F 32 -n SYSTEM uefi.raw $((262144 / 2)) >/dev/null
esp="uefi.raw@@$((2048 * 512))"
mmd -i "$esp" ::/EFI ::/EFI/Microsoft ::/EFI/Microsoft/Boot ::/EFI/Boot
mcopy -i "$esp" bootmgfw.efi ::/EFI/Microsoft/Boot/bootmgfw.efi
mcopy -i "$esp" BCD ::/EFI/Microsoft/Boot/BCD
mcopy -i "$esp" bootmgfw.efi ::/EFI/Boot/bootx64.efi

# mkntfs can't format at an offset; build the volumes, then place them.
truncate -s 64M windows.ntfs
mkntfs -F -Q -L Windows windows.ntfs >/dev/null 2>&1
truncate -s 64M recovery.ntfs
mkntfs -F -Q -L Recovery recovery.ntfs >/dev/null 2>&1
dd if=windows.ntfs of=uefi.raw bs=512 seek=296960 conv=notrunc status=none
dd if=recovery.ntfs of=uefi.raw bs=512 seek=428032 conv=notrunc status=none

# --- BIOS / MBR ---------------------------------------------------------
truncate -s 128M bios.raw
sfdisk --quiet bios.raw <<'EOF'
label: dos
unit: sectors
start=2048, size=260096, type=7, bootable
EOF
dd if=windows.ntfs of=bios.raw bs=512 seek=2048 conv=notrunc status=none

# Windows' own boot code: the MBR bootstrap's messages, and the file name
# the NTFS volume boot record chains to. mkntfs writes neither.
python3 -c "
disk = open('bios.raw', 'r+b')
mbr = bytearray(disk.read(512))
for off, msg in ((0x8b, b'Invalid partition table'),
                 (0xa3, b'Error loading operating system'),
                 (0xc1, b'Missing operating system')):
    mbr[off:off + len(msg)] = msg
disk.seek(0); disk.write(mbr)

vbr_at = 2048 * 512
disk.seek(vbr_at)
vbr = bytearray(disk.read(512))
for off, msg in ((0x188, b'BOOTMGR'),
                 (0x1a0, b'BOOTMGR is missing')):   # both inside the boot code
    vbr[off:off + len(msg)] = msg
disk.seek(vbr_at); disk.write(vbr)
disk.close()
"

# --- dual boot: the UEFI layout, plus a Linux /boot with a GRUB menu ----
mkdir -p boot/grub
for v in 6.6.9-test; do
    head -c 256K /dev/urandom > "boot/vmlinuz-$v"
    head -c 512K /dev/urandom > "boot/initramfs-$v.img"
done
# `set default` names the os-prober entry by id, i.e. GRUB boots Windows.
cat > boot/grub/grub.cfg <<'CFG'
set default="osprober-efi-1234-ABCD"
menuentry 'Debian GNU/Linux' $menuentry_id_option 'debian' {
  linux /vmlinuz-6.6.9-test root=/dev/vda5 ro quiet
  initrd /initramfs-6.6.9-test.img
}
menuentry 'Debian GNU/Linux, recovery mode' $menuentry_id_option 'debian-recovery' {
  linux /vmlinuz-6.6.9-test root=/dev/vda5 ro single
  initrd /initramfs-6.6.9-test.img
}
menuentry 'Windows Boot Manager (on /dev/vda1)' --class windows $menuentry_id_option 'osprober-efi-1234-ABCD' {
  insmod part_gpt
  insmod fat
  search --no-floppy --fs-uuid --set=root 1234-ABCD
  chainloader /EFI/Microsoft/Boot/bootmgfw.efi
}
CFG

truncate -s 512M dual.raw
dd if=uefi.raw of=dual.raw conv=notrunc status=none
sfdisk --quiet dual.raw <<'SFD'
label: gpt
unit: sectors
start=2048,   size=262144, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="EFI system partition"
start=264192, size=32768,  type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE, name="Microsoft reserved partition"
start=296960, size=131072, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="Basic data partition"
start=428032, size=131072, type=DE94BBA4-06D1-4D40-A16A-BFD50179D6AC, name="Basic data partition"
start=559104, size=262144, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="boot"
SFD
mke2fs -q -t ext4 -L bootfs -d boot -E offset=$((559104 * 512)) dual.raw 128M

for img in uefi bios dual; do
    qemu-img convert -f raw -O qcow2 "$img.raw" "$img.qcow2"
done
echo "created $dir/{uefi,bios,dual}.{raw,qcow2}"
