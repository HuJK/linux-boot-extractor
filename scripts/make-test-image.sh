#!/bin/sh
# Build a small test disk (GPT, vfat ESP + ext4 boot) in raw and qcow2 form.
# The ext4 partition has TWO kernels and a grub.cfg + grubenv where
# saved_entry pins the OLDER kernel — exercising multi-kernel default
# selection. Needs: sfdisk, mkfs.vfat, mcopy, mke2fs, qemu-img, python3.
set -eu

dir="${1:-/tmp/lbxtest}"
rm -rf "$dir"
mkdir -p "$dir/boot/grub"
cd "$dir"

for v in 6.6.30-test 6.6.9-test; do
    head -c 256K /dev/urandom > "boot/vmlinuz-$v"
    head -c 512K /dev/urandom > "boot/initramfs-$v.img"
done

cat > boot/grub/grub.cfg <<'EOF'
set default="${saved_entry}"
menuentry 'Test 6.6.30' $menuentry_id_option 'test-6.6.30' {
  linux /vmlinuz-6.6.30-test root=/dev/vda2 console=hvc0
  initrd /initramfs-6.6.30-test.img
}
menuentry 'Test 6.6.9' $menuentry_id_option 'test-6.6.9' {
  linux /vmlinuz-6.6.9-test root=/dev/vda2 console=hvc0 oldkernel
  initrd /initramfs-6.6.9-test.img
}
EOF

# grubenv is a fixed 1024-byte block padded with '#'
python3 -c "
data = b'# GRUB Environment Block\nsaved_entry=test-6.6.9\n'
open('boot/grub/grubenv', 'wb').write(data + b'#' * (1024 - len(data)))
"

truncate -s 64M disk.raw
sfdisk --quiet disk.raw <<'EOF'
label: gpt
unit: sectors
start=2048,  size=32768, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="esp"
start=34816, size=94175, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="boot"
EOF

mkfs.vfat --offset 2048 -S 512 -n EFITEST disk.raw $((32768 / 2)) >/dev/null
mcopy -i "disk.raw@@$((2048 * 512))" boot/grub/grub.cfg ::/STARTUP.CFG
mke2fs -q -t ext4 -L bootfs -d boot -E offset=$((34816 * 512)) disk.raw 45M

qemu-img convert -f raw -O qcow2 disk.raw disk.qcow2
# compressed variant + an overlay with a backing file, for the qcow2 paths
qemu-img convert -f raw -O qcow2 -c disk.raw disk-compressed.qcow2
qemu-img create -q -f qcow2 -b disk.qcow2 -F qcow2 overlay.qcow2
echo "created $dir/{disk.raw,disk.qcow2,disk-compressed.qcow2,overlay.qcow2}"
