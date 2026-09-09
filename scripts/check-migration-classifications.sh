#!/usr/bin/env bash
set -euo pipefail

root="services/control-api"
manifest="$root/migration-classifications.json"
baseline="$(jq -er '.baseline_through | numbers' "$manifest")"

jq -e '
  .migrations | type == "array" and
  (map(.version) | length == (unique | length)) and
  all(.[];
    (.version | type == "number") and
    (.classification == "expand" or .classification == "migrate" or .classification == "contract") and
    (.raises_compatibility_floor | type == "boolean") and
    (if .classification == "contract" then .raises_compatibility_floor else (.raises_compatibility_floor | not) end)
  )
' "$manifest" >/dev/null

for migration in "$root"/migrations/[0-9][0-9][0-9][0-9]_*.sql; do
  filename="${migration##*/}"
  version="$((10#${filename%%_*}))"
  if (( version <= baseline )); then
    continue
  fi
  count="$(jq --argjson version "$version" '[.migrations[] | select(.version == $version)] | length' "$manifest")"
  if [[ "$count" != 1 ]]; then
    echo "$filename must have exactly one migration classification" >&2
    exit 1
  fi
  classification="$(jq -r --argjson version "$version" '.migrations[] | select(.version == $version) | .classification' "$manifest")"
  if [[ "$classification" == contract ]] && ! grep -Eiq "UPDATE[[:space:]]+(public\.)?schema_compatibility[[:space:]]+SET[[:space:]]+minimum_migration_version[[:space:]]*=[[:space:]]*$version([^0-9]|$)" "$migration"; then
    echo "$filename is contract-classified but does not raise the compatibility floor to $version" >&2
    exit 1
  fi
done

while IFS= read -r version; do
  if ! compgen -G "$root/migrations/$(printf '%04d' "$version")_*.sql" >/dev/null; then
    echo "classification references missing migration $version" >&2
    exit 1
  fi
done < <(jq -r '.migrations[].version' "$manifest")
