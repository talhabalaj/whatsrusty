#!/usr/bin/env bash
# Regenerate AppIcon.svg, rasterize every macOS icon size, and build AppIcon.icns.
# Requires: python3, rsvg-convert (brew install librsvg), iconutil (Xcode CLT).
set -euo pipefail
cd "$(dirname "$0")"

python3 make-icon.py >/dev/null

rm -rf AppIcon.iconset png
mkdir -p AppIcon.iconset png

for size in 16 32 64 128 256 512 1024; do
  python3 make-icon.py "$size" | rsvg-convert -w "$size" -h "$size" -o "png/icon_${size}.png"
done

# iconset naming: icon_<pt>x<pt>[@2x].png
for pt in 16 32 128 256 512; do
  cp "png/icon_${pt}.png" "AppIcon.iconset/icon_${pt}x${pt}.png"
  cp "png/icon_$((pt * 2)).png" "AppIcon.iconset/icon_${pt}x${pt}@2x.png"
done

iconutil -c icns AppIcon.iconset -o AppIcon.icns
rm -rf AppIcon.iconset
echo "built $(pwd)/AppIcon.icns"
