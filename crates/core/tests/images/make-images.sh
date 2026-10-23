#!/bin/sh
# Builds the file system test images used by tests/filesystems.rs with the
# real tools of each file system, then deletes some files on them.
#
# Needs root and: dosfstools, mtools, ntfs-3g, exfatprogs, exfat-fuse.
# The images are committed, so this only has to run when they change.
set -eu

cd "$(dirname "$0")"
export TZ=UTC LANG=C.UTF-8 MTOOLS_SKIP_CHECK=1
work=$(mktemp -d)
trap 'umount "$work/mnt" 2>/dev/null || true; rm -rf "$work"' EXIT
mkdir "$work/files" "$work/mnt"

# Content of a test file: "<name>:<line number>" lines, cut to <size> bytes.
# tests/filesystems.rs generates the same content to check recovered files.
make_file() { # name size
    python3 -c 'import sys
name, size = sys.argv[1], int(sys.argv[2])
out, i = bytearray(), 0
while len(out) < size:
    out += f"{name}:{i}\n".encode(); i += 1
sys.stdout.buffer.write(out[:size])' "$1" "$2" > "$work/files/$1"
    touch -d "2024-03-15 10:30:00" "$work/files/$1"
}

make_file readme.txt 1000
make_file "holiday photo 2024.jpg" 20000
make_file "deleted picture.jpg" 15000
make_file "report final.docx" 8000
make_file notes.txt 300
make_file letter.txt 5000
make_file data.bin 12000
make_file "ünïcødé файл.txt" 2000
make_file tiny.txt 50
make_file big.bin 300000
make_file empty.txt 0

# Copies the test tree into the directory $1.
populate() {
    d=$1
    f=$work/files
    mkdir "$d/Photos" "$d/Documents" "$d/Old Stuff"
    cp -p "$f/readme.txt" "$f/ünïcødé файл.txt" "$f/tiny.txt" "$f/big.bin" "$f/empty.txt" "$d/"
    cp -p "$f/holiday photo 2024.jpg" "$f/deleted picture.jpg" "$d/Photos/"
    cp -p "$f/report final.docx" "$f/notes.txt" "$d/Documents/"
    cp -p "$f/letter.txt" "$f/data.bin" "$d/Old Stuff/"
}

# Deletes the same files on every image.
delete_files() {
    d=$1
    rm "$d/Photos/deleted picture.jpg" "$d/Documents/report final.docx" \
        "$d/ünïcødé файл.txt" "$d/tiny.txt"
    rm -r "$d/Old Stuff"
}

# FAT, using mtools (no mounting needed).
fat_image() { # name size-in-KiB mkfs-options...
    img=$work/$1
    shift
    size=$1
    shift
    mkfs.vfat -C "$@" -n TESTVOL "$img" "$size" >/dev/null
    tree=$work/tree
    rm -rf "$tree" && mkdir "$tree" && populate "$tree"
    mcopy -s -m -i "$img" "$tree"/* ::/
    mdel -i "$img" "::/Photos/deleted picture.jpg" "::/Documents/report final.docx" \
        "::/ünïcødé файл.txt" "::/tiny.txt"
    mdeltree -i "$img" "::/Old Stuff"
}

fat_image fat12.img 1440 -F 12
fat_image fat16.img 16384 -F 16
fat_image fat32.img 36864 -F 32 -s 1

# exFAT, mounted through a loop device.
truncate -s 8M "$work/exfat.img"
mkfs.exfat -L TESTVOL "$work/exfat.img" >/dev/null
loop=$(losetup -f --show "$work/exfat.img")
mount.exfat-fuse "$loop" "$work/mnt"
populate "$work/mnt"
sync
delete_files "$work/mnt"
umount "$work/mnt"
losetup -d "$loop"

# NTFS, mounted with ntfs-3g.
truncate -s 16M "$work/ntfs.img"
mkfs.ntfs -F -q -L TESTVOL "$work/ntfs.img" 2>/dev/null
ntfs-3g "$work/ntfs.img" "$work/mnt"
populate "$work/mnt"
sync
delete_files "$work/mnt"
umount "$work/mnt"

for img in fat12 fat16 fat32 exfat ntfs; do
    gzip -9 -n -c "$work/$img.img" > "$img.img.gz"
done
ls -l ./*.img.gz
