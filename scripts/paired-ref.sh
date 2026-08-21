#!/usr/bin/env bash
# Which branch of a downstream repo to test this change against.
#
# A change that spans repos is developed on a branch of the same name in each,
# so that is the whole rule: if <repo> has a branch named like this PR's head
# branch, use it; otherwise use main.
#
# This replaces a `paired-<repo>:<branch>` label. A label is metadata, so it did
# not travel with the commit, did not show in the diff, did not exist at all on
# a push, and - because labelling does not start a run and a re-run replays the
# original payload - only took effect if you remembered to close and reopen the
# PR. A branch either exists or it does not.
#
# Usage: paired-ref.sh <owner/repo> <branch>
set -euo pipefail

repo=$1
branch=${2:-}

if [ -z "$branch" ]; then
  # A push, not a pull request: there is no head branch to pair with.
  echo main
  exit 0
fi

# `git ls-remote` rather than the API: no token needed for a public repo, and it
# answers the only question being asked. Failure to reach the remote falls back
# to main rather than failing the job, since main is what would have been used
# anyway.
if git ls-remote --exit-code --heads "https://github.com/${repo}.git" "$branch" >/dev/null 2>&1; then
  echo "$branch"
else
  echo main
fi
