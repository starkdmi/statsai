#!/usr/bin/env bash
set -euo pipefail

# Checks a change to crates/statsai-pricing/pricing.json against the editing
# rules in docs/pricing.md. CI runs it on every pull request.

pricing_path=crates/statsai-pricing/pricing.json

usage() {
  cat <<'EOF' >&2
usage: scripts/check-pricing-file.sh <base-ref>
       scripts/check-pricing-file.sh --files <base.json> <head.json>

Compares crates/statsai-pricing/pricing.json at <base-ref> with the working
tree, or compares two files. Fails when the file changed and:

  - it is not valid JSON with integer `format` and `version`;
  - `version` did not go up, or `format` went down;
  - a `models` or `equivalents` entry was removed;
  - a period that has started (`from` is null or on or before today's UTC
    date) was changed or removed, in `prices`, `fast.prices` or `equivalents`.
EOF
  exit 64
}

for tool in git jq; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "check-pricing-file: $tool is required" >&2
    exit 2
  fi
done

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

case "${1:-}" in
  --files)
    [[ $# -eq 3 ]] || usage
    base_file=$2
    head_file=$3
    label=$head_file
    ;;
  "" | -*)
    usage
    ;;
  *)
    [[ $# -eq 1 ]] || usage
    base_ref=$1
    cd "$(git rev-parse --show-toplevel)"
    if ! git rev-parse --verify --quiet "$base_ref^{commit}" >/dev/null; then
      echo "check-pricing-file: cannot resolve $base_ref; fetch it first" >&2
      exit 2
    fi
    base_file=$scratch/base.json
    if ! git show "$base_ref:$pricing_path" >"$base_file" 2>/dev/null; then
      echo "$pricing_path is new at $base_ref; nothing to compare."
      exit 0
    fi
    head_file=$pricing_path
    label=$pricing_path
    ;;
esac

if [[ ! -f "$base_file" ]]; then
  echo "$base_file does not exist; nothing to compare."
  exit 0
fi
if [[ ! -f "$head_file" ]]; then
  echo "error: $label was removed. Every binary embeds it." >&2
  exit 1
fi
if cmp -s "$base_file" "$head_file"; then
  echo "$label is unchanged."
  exit 0
fi

if ! jq -e 'type == "object"' "$head_file" >/dev/null 2>&1; then
  echo "error: $label is not a valid JSON object." >&2
  exit 1
fi
if ! jq -e 'type == "object"' "$base_file" >/dev/null 2>&1; then
  echo "warning: the base file is not a valid JSON object; checking only the new file." >&2
  echo '{}' >"$scratch/base-empty.json"
  base_file=$scratch/base-empty.json
fi

today=$(date -u +%F)

# Prints one violation per output string.
violations=$(jq -r --slurpfile base "$base_file" --arg today "$today" '
  def isint: type == "number" and . == floor;
  def obj: if type == "object" then . else {} end;
  def arr: if type == "array" then . else [] end;
  def started: .from == null or ((.from | type) == "string" and .from <= $today);
  def from_label: if .from == null then "null" else .from end;

  # Every started period in $old must be in $new, unchanged, identified by `from`.
  def kept_periods($where; $old; $new):
    ($new | arr) as $new
    | ($old | arr)[]
    | select(type == "object" and started) as $period
    | [$new[] | select(type == "object" and .from == $period.from)] as $matches
    | if ($matches | length) == 0 then
        "\($where): the period from \($period | from_label) was removed. It has started, so it must stay as it is; add a new period to change the price."
      elif any($matches[]; . == $period) then
        empty
      else
        "\($where): the period from \($period | from_label) was changed. It has started, so it must stay as it is; add a new period to change the price.\n  before: \($period | tojson)\n  after:  \($matches[0] | tojson)"
      end;

  ($base[0] | obj) as $base
  | . as $head
  | (
      if ($head.format | isint) | not then
        "format: must be an integer."
      elif ($base.format | isint) and $head.format < $base.format then
        "format: went down from \($base.format) to \($head.format). It never goes down."
      else empty end
    ),
    (
      if ($head.version | isint) | not then
        "version: must be an integer."
      elif ($base.version | isint) and $head.version <= $base.version then
        "version: is \($head.version), not higher than the base version \($base.version). Every change to the file raises it; set it to \($base.version + 1) or higher."
      else empty end
    ),
    (
      ($base.models | obj) as $old
      | ($head.models | obj) as $new
      | $old | keys_unsorted[]
      | . as $name
      | if ($new | has($name)) | not then
          "models.\($name): removed. Stored events still name it; keep the entry."
        else
          kept_periods("models.\($name).prices"; ($old[$name] | obj).prices; ($new[$name] | obj).prices),
          kept_periods("models.\($name).fast.prices";
            ($old[$name] | obj | .fast | obj).prices; ($new[$name] | obj | .fast | obj).prices)
        end
    ),
    (
      ($base.equivalents | obj) as $old
      | ($head.equivalents | obj) as $new
      | $old | keys_unsorted[]
      | . as $name
      | if ($new | has($name)) | not then
          "equivalents.\($name): removed. Stored events still name it; keep the entry."
        else
          kept_periods("equivalents.\($name)"; $old[$name]; $new[$name])
        end
    )
' "$head_file")

if [[ -n "$violations" ]]; then
  echo "$label breaks the pricing file rules (see docs/pricing.md):" >&2
  while IFS= read -r line; do
    echo "  $line" >&2
  done <<<"$violations"
  exit 1
fi

echo "$label: version $(jq -r '.version' "$base_file") -> $(jq -r '.version' "$head_file"); rules hold."
