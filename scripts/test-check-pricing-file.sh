#!/usr/bin/env bash
set -euo pipefail

# Tests scripts/check-pricing-file.sh: runs it with --files on edited copies of
# crates/statsai-pricing/pricing.json, and with a git ref, as CI runs it, in a
# scratch repository, and checks its exit status and message for each case.
# The "Pricing file" CI job runs it before the check itself.
#
# The edits target entries this script adds to its own copy (`test-model`,
# `test-alias`, `test-today`), with periods that started long ago (null,
# 2001-01-01), one that starts today and one that has not started
# (2999-01-01), so the cases hold whatever prices pricing.json holds and
# whatever day the test runs.

# A git hook exports its repository (GIT_DIR, GIT_INDEX_FILE, ...); the scratch
# repository below must not use it.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY \
  GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_PREFIX GIT_NAMESPACE GIT_CEILING_DIRECTORIES

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
check=$root/scripts/check-pricing-file.sh
pricing=$root/crates/statsai-pricing/pricing.json

for tool in git jq; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "test-check-pricing-file: $tool is required" >&2
    exit 2
  fi
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

passed=0
failed=0

# run_case <case> <exit status> <text the output contains> <command...>
run_case() {
  local name=$1 want_status=$2 needle=$3
  shift 3
  local output status=0
  output=$("$@" 2>&1) || status=$?
  if [[ $status -ne $want_status ]]; then
    echo "FAIL $name: exit status $status, expected $want_status" >&2
  elif [[ $output != *"$needle"* ]]; then
    echo "FAIL $name: the output does not contain: $needle" >&2
  else
    echo "ok   $name"
    passed=$((passed + 1))
    return
  fi
  echo "$output" | sed 's/^/     | /' >&2
  failed=$((failed + 1))
}

# expect <case> <exit status> <text the output contains> <base file> <head file>
expect() {
  run_case "$1" "$2" "$3" "$check" --files "$4" "$5"
}

# head_file <name> <jq filter> [<file>]: writes the file (the base file by
# default) edited by the filter and prints its path.
head_file() {
  local path=$work/$1.json
  jq "$2" "${3:-$base}" >"$path"
  echo "$path"
}

rates() {
  printf '"input": %s, "cache_write": %s, "cache_read": %s, "output": %s' "$1" "$1" "$1" "$1"
}

first_model=$(jq -r '.models | keys_unsorted[0]' "$pricing")
base=$work/base.json
jq --arg target "$first_model" "
  .models[\"test-model\"] = {
    \"prices\": [
      { \"from\": null, $(rates 1) },
      { \"from\": \"2001-01-01\", $(rates 2) },
      { \"from\": \"2999-01-01\", $(rates 3) }
    ],
    \"fast\": {
      \"prices\": [
        { \"from\": null, $(rates 4) },
        { \"from\": \"2001-01-01\", $(rates 5) },
        { \"from\": \"2999-01-01\", $(rates 6) }
      ]
    }
  }
  | .equivalents[\"test-alias\"] = [
      { \"from\": null, \"model\": \"test-model\" },
      { \"from\": \"2001-01-01\", \"model\": \$target },
      { \"from\": \"2999-01-01\", \"model\": \"test-model\" }
    ]
" "$pricing" >"$base"
version=$(jq '.version' "$base")
format=$(jq '.format' "$base")
bump='.version += 1'
today=$(date -u +%F)

# Files that pass.
cp "$base" "$work/same.json"
expect "unchanged file" 0 "is unchanged." "$base" "$work/same.json"
expect "unchanged pricing.json" 0 "is unchanged." "$pricing" "$pricing"
expect "a model added, version raised" 0 "version $version -> $((version + 1)); rules hold." \
  "$base" "$(head_file added-model "$bump | .models[\"test-added\"] = { \"prices\": [ { \"from\": null, $(rates 7) } ] }")"
expect "a period added after the started ones" 0 "rules hold." \
  "$base" "$(head_file added-period "$bump | .models[\"test-model\"].prices += [ { \"from\": \"3000-01-01\", $(rates 8) } ]")"
expect "a period that has not started edited in prices" 0 "rules hold." \
  "$base" "$(head_file future-prices "$bump | .models[\"test-model\"].prices[2].input = 9")"
expect "a period that has not started edited in fast.prices" 0 "rules hold." \
  "$base" "$(head_file future-fast "$bump | .models[\"test-model\"].fast.prices[2].output = 9")"
expect "a period that has not started edited in equivalents" 0 "rules hold." \
  "$base" "$(head_file future-equivalent "$bump | .equivalents[\"test-alias\"][2].model = \"$first_model\"")"
expect "a period that has not started removed" 0 "rules hold." \
  "$base" "$(head_file future-removed "$bump | del(.models[\"test-model\"].prices[2])")"
expect "version raised by the most allowed" 0 "version $version -> $((version + 10)); rules hold." \
  "$base" "$(head_file step-10 '.version += 10')"
near_ceiling=$(head_file near-ceiling '.version = 999995')
expect "version raised to 1000000" 0 "version 999995 -> 1000000; rules hold." \
  "$near_ceiling" "$(head_file ceiling '.version = 1000000' "$near_ceiling")"
expect "no base file" 0 "does not exist; nothing to compare." "$work/missing.json" "$base"

# Files that fail.
expect "a change without a version bump" 1 \
  "version: is $version, not higher than the base version $version." \
  "$base" "$(head_file no-bump '.models["test-model"].note = "changed"')"
expect "version lowered" 1 \
  "version: is $((version - 1)), not higher than the base version $version." \
  "$base" "$(head_file lowered '.version -= 1 | .models["test-model"].note = "changed"')"
expect "version not an integer" 1 "version: must be an integer." \
  "$base" "$(head_file version-string '.version = "8"')"
expect "version raised by more than 10" 1 \
  "version: is $((version + 11)), more than 10 above the base version $version." \
  "$base" "$(head_file step-11 '.version += 11')"
expect "a zero typed after the next version" 1 \
  "version: is $(((version + 1) * 10)), more than 10 above the base version $version." \
  "$base" "$(head_file step-typo "$bump | .version *= 10")"
expect "version raised to the largest 64-bit integer" 1 \
  "more than 10 above the base version $version." \
  "$base" "$(head_file version-max '.version = 18446744073709551615')"
expect "version raised within the step to above 1000000" 1 \
  "version: is 1000001, above 1000000." \
  "$near_ceiling" "$(head_file above-ceiling '.version = 1000001' "$near_ceiling")"
printf 'not json\n' >"$work/not-json.json"
expect "version above 1000000 over an unreadable base" 1 \
  "version: is 2000000, above 1000000." \
  "$work/not-json.json" "$(head_file version-ceiling '.version = 2000000')"
expect "format not an integer" 1 "format: must be an integer." \
  "$base" "$(head_file format-string "$bump | .format = \"$format\"")"
expect "format going down" 1 "format: went down from $format to $((format - 1)). It never goes down." \
  "$base" "$(head_file format-down "$bump | .format -= 1")"
expect "a model removed" 1 "models.$first_model: removed. Stored events still name it; keep the entry." \
  "$base" "$(head_file removed-model "$bump | del(.models[\"$first_model\"])")"
expect "an equivalents entry removed" 1 "equivalents.test-alias: removed. Stored events still name it; keep the entry." \
  "$base" "$(head_file removed-equivalent "$bump | del(.equivalents[\"test-alias\"])")"
expect "the first period edited in prices" 1 \
  "models.test-model.prices: the period from null was changed. It has started, so it must stay as it is" \
  "$base" "$(head_file started-null "$bump | .models[\"test-model\"].prices[0].cache_read = 0.5")"
expect "a dated started period edited in prices" 1 \
  "models.test-model.prices: the period from 2001-01-01 was changed." \
  "$base" "$(head_file started-dated "$bump | .models[\"test-model\"].prices[1].output = 9")"
starts_today=$(head_file starts-today "
  .models[\"test-today\"] = {
    \"prices\": [ { \"from\": null, $(rates 1) }, { \"from\": \"$today\", $(rates 2) } ]
  }")
expect "a period that starts today edited" 1 \
  "models.test-today.prices: the period from $today was changed." \
  "$starts_today" "$(head_file today-edited "$bump | .models[\"test-today\"].prices[1].input = 9" "$starts_today")"
expect "a started period removed from prices" 1 \
  "models.test-model.prices: the period from 2001-01-01 was removed." \
  "$base" "$(head_file started-removed "$bump | del(.models[\"test-model\"].prices[1])")"
expect "a started period edited in fast.prices" 1 \
  "models.test-model.fast.prices: the period from 2001-01-01 was changed." \
  "$base" "$(head_file started-fast "$bump | .models[\"test-model\"].fast.prices[1].input = 9")"
expect "a started period edited in equivalents" 1 \
  "equivalents.test-alias: the period from 2001-01-01 was changed." \
  "$base" "$(head_file started-equivalent "$bump | .equivalents[\"test-alias\"][1].model = \"test-model\"")"
expect "a started period removed from equivalents" 1 \
  "equivalents.test-alias: the period from null was removed." \
  "$base" "$(head_file started-equivalent-removed "$bump | del(.equivalents[\"test-alias\"][0])")"
expect "a file that is not JSON" 1 "is not a valid JSON object." "$base" "$work/not-json.json"
expect "the file removed" 1 "was removed. Every binary embeds it." "$base" "$work/missing.json"

# With a git ref, as CI runs it: the base is the file at the ref, and the head
# is the file in the working tree, here the commit after it.
repo=$work/repo
repo_git() {
  git -C "$repo" -c user.name=test -c user.email=test@example.invalid \
    -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"
}
# check_ref <ref>: runs the check from the scratch repository.
check_ref() {
  cd "$repo" && "$check" "$1"
}
expect_ref() {
  run_case "$1" "$2" "$3" check_ref "$4"
}
commit_pricing() {
  cp "$1" "$repo/crates/statsai-pricing/pricing.json"
  repo_git add crates/statsai-pricing/pricing.json
  repo_git commit -q -m "$2"
}
mkdir -p "$repo/crates/statsai-pricing"
git -c init.defaultBranch=main init -q "$repo"
echo scratch >"$repo/README"
repo_git add README
repo_git commit -q -m "before the pricing file"
repo_git tag before-file
commit_pricing "$base" "add the pricing file"
commit_pricing "$(head_file ref-bumped "$bump")" "raise the version"
expect_ref "git ref: a change with a raised version" 0 \
  "crates/statsai-pricing/pricing.json: version $version -> $((version + 1)); rules hold." "HEAD^1"
commit_pricing "$(head_file ref-unbumped "$bump | .models[\"test-model\"].note = \"changed\"")" \
  "change without raising the version"
expect_ref "git ref: a change without a version bump" 1 \
  "version: is $((version + 1)), not higher than the base version $((version + 1))." "HEAD^1"
expect_ref "git ref: the file is new at the ref" 0 \
  "crates/statsai-pricing/pricing.json is new at before-file; nothing to compare." before-file
expect_ref "git ref: a ref that does not resolve" 2 "cannot resolve no-such-ref; fetch it first" no-such-ref

echo "$passed passed, $failed failed"
[[ $failed -eq 0 ]]
