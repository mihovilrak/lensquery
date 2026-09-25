#!/usr/bin/env bash
# Fail the build if anything private leaked into the tree.
#
# This repo was started fresh precisely so that no private data is recoverable
# from history. That guarantee only holds if nothing private gets committed
# going forward, so this runs in `just check` and in CI, not just once.
#
# Scan the complete commit candidate: tracked files plus untracked files that
# are not ignored. This matters before a fresh root commit, when most of the
# intended public tree has not been added yet.
set -uo pipefail

cd "$(dirname "$0")/.."

status=0

# Personal paths and machine-specific absolute locations.
patterns=(
  'C:\\+Users\\+Mihovil'       # the author's Windows home directory
  'lens-query'                  # the private predecessor repo
)

for p in "${patterns[@]}"; do
  if hits=$(git grep --untracked -nIE "$p" -- . ':!scripts/check-privacy.sh' 2>/dev/null); then
    echo "FAIL: private path pattern /$p/"
    echo "$hits"
    status=1
  fi
done

# Corpus filenames from the author's private test set. These are the shapes
# that showed up in the predecessor repo's docs; they must never reappear.
corpus=(
  'IMG-[0-9]{8}-WA[0-9]{4}'     # WhatsApp media
  'Screenshot_[0-9]{8}-[0-9]{6}'
  '[0-9]{9,}_[0-9]{9,}_[0-9]{9,}'  # social-media asset ids
)

for p in "${corpus[@]}"; do
  content_hits=$(git grep --untracked -nIE "$p" -- . ':!scripts/check-privacy.sh' 2>/dev/null || true)
  filename_hits=$(git ls-files --cached --others --exclude-standard | grep -E "$p" || true)
  hits=$(printf '%s\n%s\n' "$content_hits" "$filename_hits" | sed '/^$/d' | sort -u)
  if [ -n "$hits" ]; then
    echo "FAIL: private corpus filename pattern /$p/"
    echo "$hits"
    status=1
  fi
done

# Files that must never be tracked regardless of content: language models are
# large binaries fetched at runtime, ground-truth labels are derived from a
# private corpus, and databases contain whatever the author indexed.
forbidden='\.(traineddata|db|db-wal|db-shm)$|^(gtd|test-data|results|tessdata|tessdata-best|dist|dist-rust)/'
if hits=$(git ls-files --cached --others --exclude-standard | grep -E "$forbidden"); then
  echo "FAIL: forbidden files are in the commit candidate"
  echo "$hits"
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "privacy scan: clean ($(git ls-files --cached --others --exclude-standard | wc -l) candidate files)"
fi

exit "$status"
