#!/usr/bin/env bash
# Downloads the sample files listed in a manifest (manifest.tsv, the .3dm samples, by default;
# ifc.tsv for the IFC samples) from their pinned upstream commits and verifies each one against
# its SHA-256. Valid files already present are skipped.
#   samples/fetch.sh [manifest.tsv | ifc.tsv]
set -euo pipefail
cd "$(dirname "$0")"
manifest=${1:-manifest.tsv}

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

fetched=0
skipped=0
while IFS=$'\t' read -r path repo commit upstream _bytes sum; do
  [[ -z $path || $path == \#* ]] && continue
  if [[ -f $path && $(sha256 "$path") == "$sum" ]]; then
    skipped=$((skipped + 1))
    continue
  fi
  mkdir -p "$(dirname "$path")"
  curl -fsSL --retry 3 -o "$path.part" "https://raw.githubusercontent.com/$repo/$commit/${upstream// /%20}"
  if [[ $(sha256 "$path.part") != "$sum" ]]; then
    rm -f "$path.part"
    echo "checksum mismatch: $path" >&2
    exit 1
  fi
  mv "$path.part" "$path"
  fetched=$((fetched + 1))
done < "$manifest"

echo "samples: $fetched fetched, $skipped already present"
