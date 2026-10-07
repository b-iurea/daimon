#!/bin/sh
# Build Daimon: one UEFI kernel (initramfs + cmdline built in) on a GPT disk image.
#   ./build.sh          -> out/daimon.qcow2 (import into Proxmox / any UEFI VM)
#   ./build.sh run      -> build, then boot the image in QEMU+OVMF (screen in a window, API on host :8080)
set -eu
cd "$(dirname "$0")"
ROOT=$PWD
K=$ROOT/build/linux-6.18.55
OUT=$ROOT/out
DISK=${DISK:-16G}
mkdir -p "$OUT"

# --- userspace
cargo build -q --release --target x86_64-unknown-linux-musl --manifest-path aios/Cargo.toml
strip -o "$OUT/llama-server" build/llama.cpp/build-cpu/bin/llama-server
# console assets, rebuilt only when their generator changes (need ckbcomp + python3-pil on the host)
[ "$OUT/keymaps/index.txt" -nt tools/mkkeymaps.py ] || python3 tools/mkkeymaps.py "$OUT/keymaps"
[ "$OUT/fonts/.done" -nt tools/mkfont.py ] || { python3 tools/mkfont.py "$OUT/fonts" && touch "$OUT/fonts/.done"; }
[ "$OUT/logo.alf" -nt tools/mklogo.py ] || python3 tools/mklogo.py "$OUT/logo.alf"

mod() { # mod <name> <cmd> [tty]
  mkdir -p "$OUT/rootfs/$1"
  echo "$2" > "$OUT/rootfs/$1/cmd"
  echo "dir /etc/aios/modules/$1 0755 0 0"
  echo "file /etc/aios/modules/$1/cmd $OUT/rootfs/$1/cmd 0644 0 0"
  if [ -n "${3:-}" ]; then
    echo "$3" > "$OUT/rootfs/$1/tty"
    echo "file /etc/aios/modules/$1/tty $OUT/rootfs/$1/tty 0644 0 0"
  fi
}
{
  cat <<EOF
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
dir /proc 0755 0 0
dir /sys 0755 0 0
dir /run 0755 0 0
dir /tmp 1777 0 0
dir /data 0755 0 0
dir /etc 0755 0 0
dir /etc/aios 0755 0 0
dir /etc/aios/modules 0755 0 0
dir /usr 0755 0 0
dir /usr/bin 0755 0 0
file /init $ROOT/aios/target/x86_64-unknown-linux-musl/release/aios 0755 0 0
slink /usr/bin/aios /init 0777 0 0
file /usr/bin/llama-server $OUT/llama-server 0755 0 0
dir /usr/share 0755 0 0
dir /usr/share/aios 0755 0 0
dir /usr/share/aios/keymaps 0755 0 0
dir /usr/share/aios/fonts 0755 0 0
EOF
  for f in "$OUT"/keymaps/*; do echo "file /usr/share/aios/keymaps/${f##*/} $f 0644 0 0"; done
  echo "file /usr/share/aios/logo.alf $OUT/logo.alf 0644 0 0"
  for f in "$OUT"/fonts/*.fnt; do echo "file /usr/share/aios/fonts/${f##*/} $f 0644 0 0"; done
  mod llm "/usr/bin/aios llm"
  mod judge "/usr/bin/aios judge"
  mod tui "/usr/bin/aios tui" tty1
} > "$OUT/initramfs.list"

# --- kernel (relinks in seconds when only the initramfs changed)
cd "$K"
make -s defconfig && make -s kvm_guest.config >/dev/null
scripts/kconfig/merge_config.sh -m .config "$ROOT/kernel/aios.config" >/dev/null
scripts/config --set-str INITRAMFS_SOURCE "$OUT/initramfs.list"
make -s olddefconfig
make -s -j"$(nproc)" bzImage
cd "$ROOT"

# --- disk: GPT, 64M EFI system partition + ext4 "aios-data" with the models
IMG=$OUT/daimon.raw
rm -f "$IMG"
truncate -s "$DISK" "$IMG"
sfdisk -q "$IMG" <<EOF
label: gpt
start=1MiB, size=64MiB, type=uefi, name=EFI
start=65MiB, type=linux, name=aios-data
EOF
ESP=$OUT/esp.img
rm -f "$ESP"
mkfs.vfat -C "$ESP" $((64 * 1024)) >/dev/null
mmd -i "$ESP" ::/EFI ::/EFI/BOOT
mcopy -i "$ESP" "$K/arch/x86/boot/bzImage" ::/EFI/BOOT/BOOTX64.EFI
dd if="$ESP" of="$IMG" bs=1M seek=1 conv=notrunc,sparse status=none
DATA_KB=$(( $(stat -c %s "$IMG") / 1024 - 65 * 1024 - 1024 )) # leave room for the backup GPT
mke2fs -q -t ext4 -L aios-data -d build/data -E offset=$((65 * 1024 * 1024)) "$IMG" "${DATA_KB}k"
qemu-img convert -O qcow2 "$IMG" "$OUT/daimon.qcow2"
rm -f "$IMG" "$ESP"

ls -la "$K/arch/x86/boot/bzImage" "$OUT/daimon.qcow2"

[ "${1:-}" = run ] && exec qemu-system-x86_64 -enable-kvm -cpu host -smp 4 -m 6G \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive file="$OUT/daimon.qcow2",if=virtio -snapshot \
  -nic user,model=virtio-net-pci,hostfwd=tcp::8080-:8080 -serial stdio
true
