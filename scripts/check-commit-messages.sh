#!/usr/bin/env bash
set -euo pipefail

base_sha=${1:?usage: check-commit-messages.sh <base-sha> <head-sha>}
head_sha=${2:?usage: check-commit-messages.sh <base-sha> <head-sha>}
pattern='^[a-z][a-z0-9-]*(\([[:alnum:]._/-]+\))?!?: .+'

git rev-parse --verify --quiet "${head_sha}^{commit}" >/dev/null
if [[ "$base_sha" =~ ^0+$ ]]; then
  commits=$(git rev-list --reverse "$head_sha")
else
  git rev-parse --verify --quiet "${base_sha}^{commit}" >/dev/null
  commits=$(git rev-list --reverse "$base_sha..$head_sha")
fi

failed=0
while IFS= read -r commit; do
  [[ -n "$commit" ]] || continue
  parents=$(git show -s --format=%P "$commit")
  [[ "$parents" == *" "* ]] && continue
  subject=$(git show -s --format=%s "$commit")
  if [[ ! "$subject" =~ $pattern ]]; then
    printf 'Invalid Conventional Commit subject: %s (%s)\n' "$subject" "$commit" >&2
    failed=1
  fi
done <<< "$commits"

exit "$failed"
