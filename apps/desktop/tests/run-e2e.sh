#!/bin/sh
# Runs the end-to-end test of the desktop app on Linux: builds a demo SD card
# with photos, a video and documents, deletes some of them, attaches it as a
# drive and lets e2e.py use the app like a person would.
#
# Needs root (for the loop device) and: Xvfb, WebKitWebDriver
# (webkit2gtk-driver), tauri-driver (`cargo install tauri-driver`),
# python3-selenium, dosfstools, mtools, imagemagick, ffmpeg.
# Usage: run-e2e.sh [app binary]   (default: target/debug/full-recovery)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
app=${1:-$root/target/debug/full-recovery}
work=$(mktemp -d)
export TZ=UTC MTOOLS_SKIP_CHECK=1 DISPLAY=:97

cleanup() {
    [ -n "${driver:-}" ] && kill "$driver" 2>/dev/null
    [ -n "${xvfb:-}" ] && kill "$xvfb" 2>/dev/null
    [ -n "${loop:-}" ] && losetup -d "$loop"
    rm -rf "$work"
}
trap cleanup EXIT

# Demo files.
tree=$work/tree
mkdir -p "$tree/DCIM/Camera" "$tree/Documents" "$tree/Music" "$tree/Videos"
i=0
for colors in "#ff7a59-#ffd166" "#06d6a0-#118ab2" "#8338ec-#3a86ff" "#ef476f-#ffd166" \
    "#2a9d8f-#e9c46a" "#264653-#e76f51" "#3a0ca3-#f72585" "#4cc9f0-#4361ee"; do
    i=$((i + 1))
    convert -seed $i -size 1600x1200 "plasma:$colors" -blur 0x8 -quality 85 \
        "$tree/DCIM/Camera/IMG_2024031${i}_10$((20 + i)).jpg"
done
convert -seed 42 -size 900x900 "plasma:#ffbe0b-#fb5607" -blur 0x4 "$tree/DCIM/Camera/Screenshot_2024-05-02.png"
ffmpeg -loglevel error -f lavfi -i testsrc2=duration=4:size=640x360:rate=25 \
    -f lavfi -i sine=frequency=440:duration=4 -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest \
    "$tree/Videos/birthday_party.mp4"
ffmpeg -loglevel error -f lavfi -i sine=frequency=330:duration=6 -c:a libmp3lame "$tree/Music/voice_note.mp3"
convert -size 1240x1754 xc:white -fill "#1e3a8a" -draw "rectangle 0,0 1240,180" "$tree/Documents/Invoice_March.pdf"
printf 'Shopping list\n- milk\n- bread\n- coffee\n' > "$tree/Documents/shopping-list.txt"
printf 'Thesis notes\n\nChapter 1: Introduction\nThe aim of this work is...\n' > "$tree/Documents/thesis-notes.txt"
python3 -c "
import zipfile
with zipfile.ZipFile('$tree/Documents/Project plan.docx', 'w', zipfile.ZIP_DEFLATED) as z:
    z.writestr('[Content_Types].xml', '<Types/>')
    z.writestr('word/document.xml', '<w:document>' + 'plan ' * 3000 + '</w:document>')"

touch -d "2024-03-15 10:30" "$tree"/DCIM/Camera/*
touch -d "2024-04-20 09:05" "$tree"/Documents/*
touch -d "2024-06-01 18:12" "$tree"/Videos/* "$tree"/Music/*

# The SD card: copy everything, then delete some files.
card=$work/sdcard.img
mkfs.vfat -C -F 32 -n "SD CARD" "$card" 65536 >/dev/null
mcopy -s -m -i "$card" "$tree"/* ::/
mdel -i "$card" ::/DCIM/Camera/IMG_20240312_1022.jpg ::/DCIM/Camera/IMG_20240314_1024.jpg \
    ::/DCIM/Camera/IMG_20240315_1025.jpg ::/DCIM/Camera/IMG_20240317_1027.jpg \
    ::/DCIM/Camera/Screenshot_2024-05-02.png "::/Documents/Project plan.docx" \
    ::/Documents/thesis-notes.txt ::/Videos/birthday_party.mp4
loop=$(losetup -f --show "$card")

Xvfb :97 -screen 0 1400x900x24 >/dev/null 2>&1 &
xvfb=$!
tauri-driver >/dev/null 2>&1 &
driver=$!
sleep 2

shots=${E2E_SHOTS:-$root/target/e2e}
rm -rf "$shots"
python3 "$here/e2e.py" "$app" "$shots" "$(basename "$loop")" "$tree"
