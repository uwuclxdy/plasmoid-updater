#!/usr/bin/env bash
# any step failing must stop the script: every failure it expects is caught
# below and answered as `unknown`
set -euo pipefail

# The one crates.io question release.sh and .github/workflows/release.yml's
# validate and crates jobs all ask, so the answer has one implementation and
# cannot drift apart: is crates.io's copy of `<crate>-<version>` packed from
# this checkout's HEAD commit? crates.io versions are immutable, so a version
# packed from another commit can never be released again from this tree: the
# GitHub release's binaries and crates.io's crate would disagree forever.
#
# usage: crates-io-copy.sh <crate> <version>
#
# downloads https://static.crates.io/crates/<crate>/<crate>-<version>.crate,
# reads the .cargo_vcs_info.json cargo packs into every published tarball and
# compares its git.sha1 with the cwd repo's HEAD. Prints exactly one line on
# stdout and exits 0:
#   absent            the version is not on crates.io: the download got an
#                     HTTP 403 or 404 (the CDN answers 403 for a version or a
#                     name crates.io lacks, measured 2026-11-07)
#   same              its git.sha1 is HEAD and the packed tree was clean
#   other <sha1>      its git.sha1 is <sha1>, another commit, whether or not
#                     the packed tree was dirty
#   unknown <reason>  anything else: any other HTTP status (a 5xx outage) or
#                     download failure, an unreadable record or sha1, or a
#                     dirty tree at HEAD (a dirty copy of this commit proves
#                     nothing about its bytes)
# exits non-zero on its own usage error, or when it cannot make its temp file
# (mktemp missing or TMPDIR unwritable), which every caller treats as a failed
# check. curl's stderr is not hidden, so the download's own reason
# (curl: (22) ... 503, curl: (7) ...) shows in the caller's log; a download that stalls longer than the --max-time reads as
# `unknown` instead of hanging the caller.

if [ "$#" -ne 2 ]; then
  echo "usage: crates-io-copy.sh <crate> <version>" >&2
  exit 2
fi
crate="$1"
version="$2"

if ! head_sha1="$(git rev-parse HEAD)"; then
  echo "unknown git rev-parse HEAD failed (run from a git checkout)"
  exit 0
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# -f maps every HTTP status of 400 or more to exit 22, so the status -w prints
# is what tells a missing version from an outage; a transport failure prints
# 000 or nothing.
curl_status=0
http_code="$(curl -sSfL --max-time 30 -o "$tmp" -w '%{http_code}' \
  "https://static.crates.io/crates/${crate}/${crate}-${version}.crate")" \
  || curl_status=$?
if [ "$curl_status" -eq 22 ] && { [ "$http_code" = 403 ] || [ "$http_code" = 404 ]; }; then
  echo "absent"
  exit 0
fi
if [ "$curl_status" -ne 0 ]; then
  echo "unknown curl exit ${curl_status} (HTTP ${http_code:-none})"
  exit 0
fi

if ! vcs_info="$(tar -xzOf "$tmp" "${crate}-${version}/.cargo_vcs_info.json")"; then
  echo "unknown no .cargo_vcs_info.json in ${crate}-${version}.crate"
  exit 0
fi

# only a hex object id (40 digits, 64 in a sha256 repo) is a commit: anything
# else is unreadable and never reaches the callers' messages
if ! sha1="$(jq -er '.git.sha1' <<<"$vcs_info")" \
  || ! [[ "$sha1" =~ ^[0-9a-f]{40}([0-9a-f]{24})?$ ]]; then
  echo "unknown ${crate}-${version}.crate's .cargo_vcs_info.json carries no readable commit"
  exit 0
fi

# packed from another commit, dirty or not, it can never be this tree's
if [ "$sha1" != "$head_sha1" ]; then
  echo "other ${sha1}"
  exit 0
fi

# at HEAD only a clean tree proves the bytes; a missing or null dirty flag is
# clean (a clean copy carries none, measured 2026-11-07)
if ! clean="$(jq -r '(.git.dirty // false) == false' <<<"$vcs_info")" \
  || [ "$clean" != true ]; then
  echo "unknown ${crate}-${version}.crate was packed from a dirty tree at this commit"
  exit 0
fi
echo "same"
