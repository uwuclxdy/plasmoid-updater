#!/usr/bin/env bash
set -euo pipefail

# Shared semver-guard logic for release.sh (which sources this file),
# .github/workflows/release.yml's validate job and .github/workflows/ci.yml's
# semver job (which run it directly), so the baseline rule, the release-type
# computation and the check invocation have one implementation and cannot drift
# apart.
#
# The baseline is read from origin, never from crates.io (release.yml publishes
# a version only after its tag, so a failed publish leaves crates.io behind
# origin): the newest `vX.Y.Z` tag on origin whose commit is an ancestor of
# HEAD and whose version is below the requested one (or at it, for a report on
# a manifest that is not bumped yet).
#
# Direct modes:
#   semver-guard.sh baseline <version> [below|upto]  prints the baseline tag
#   semver-guard.sh release-type <base> <cur>        prints major|minor|patch
#   semver-guard.sh check <base-tag> <type>          runs cargo-semver-checks
#   semver-guard.sh manifest-version                 prints the workspace version
#   semver-guard.sh report                           prints the release type the
#                                                    change needs; fails only when
#                                                    the manifest bump is too small

# The crate whose public API is a contract (topgrade depends on it).
SEMVER_PACKAGE="libplasmoid-updater"

# The toolchain that builds the rustdoc JSON cargo-semver-checks reads, which
# reads only the formats of the toolchains it was released against: the tool
# version and this toolchain move together. release.yml's validate job and
# ci.yml's semver job pin the same toolchain; a test holds the three equal.
SEMVER_TOOLCHAIN="1.99.1"

# resolve_baseline / previous_release_tag exit codes beyond 0:
#   1 a git command failed, 3 a fetch is needed first, 4 no baseline tag,
#   5 the baseline tag has no SEMVER_PACKAGE manifest.

version_lt() {
  # true (0) when $1 is strictly less than $2 by semver ordering; tail reads
  # the whole two-line list, so sort never gets a SIGPIPE under pipefail.
  [ "$1" = "$2" ] && return 1
  [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n1)" = "$2" ]
}

release_type() {
  # The minimum semver change between the baseline version $1 and the current
  # version $2, computed the way cargo-semver-checks v0.51.0 does
  # (classify_minimum_semver_version_change in check-release.rs): a 0.x minor
  # or patch bump is a major or minor release respectively.
  local base_major base_minor base_patch cur_major cur_minor cur_patch
  IFS=. read -r base_major base_minor base_patch <<<"$1"
  IFS=. read -r cur_major cur_minor cur_patch <<<"$2"
  if [ "$base_major" != "$cur_major" ]; then
    echo "major"
  elif [ "$base_minor" != "$cur_minor" ]; then
    if [ "$cur_major" = "0" ]; then echo "major"; else echo "minor"; fi
  elif [ "$base_patch" != "$cur_patch" ]; then
    if [ "$cur_major" = "0" ] && [ "$cur_minor" = "0" ]; then echo "major"
    elif [ "$cur_major" = "0" ]; then echo "minor"
    else echo "patch"; fi
  else
    echo "patch"
  fi
}

release_label() {
  # "a <type> release" for a message about the baseline version $2. Below 1.0
  # the types shift down a part (see release_type), so the label says what the
  # bump is.
  local major minor
  IFS=. read -r major minor _ <<<"$2"
  case "$1:$major:$minor" in
    minor:0:*) echo "a minor release (a patch bump while below 1.0)" ;;
    major:0:0) echo "a major release (any bump while at 0.0.x)" ;;
    major:0:*) echo "a major release (a minor bump while below 1.0)" ;;
    *) echo "a $1 release" ;;
  esac
}

type_rank() {
  # The order of release types: patch < minor < major.
  case "$1" in
    patch) echo 0 ;;
    minor) echo 1 ;;
    major) echo 2 ;;
    *) return 1 ;;
  esac
}

needed_version() {
  # The smallest version of release type $2 (major|minor) that announces the
  # change from the baseline $1. A major break below 1.0 is announced by the
  # next minor (0.x), at or above 1.0 by the next major. A part too long to
  # compute with is refused with a message, never an empty answer.
  local major minor kind="${2:-major}"
  IFS=. read -r major minor _ <<<"$1"
  major="${major#"${major%%[!0]*}"}"
  minor="${minor#"${minor%%[!0]*}"}"
  major="${major:-0}"
  minor="${minor:-0}"
  if [ "${#major}" -gt 18 ] || [ "${#minor}" -gt 18 ]; then
    echo "Error: version ${1} has a part over 18 digits; the next version cannot be computed from it" >&2
    return 1
  fi
  if [ "$kind" = "minor" ]; then
    echo "$major.$(( 10#$minor + 1 )).0"
  elif [ "$major" -eq 0 ]; then
    echo "0.$(( 10#$minor + 1 )).0"
  else
    echo "$(( 10#$major + 1 )).0.0"
  fi
}

origin_release_tags() {
  # One "<X.Y.Z> <commit sha>" line per vX.Y.Z tag on origin, newest first. An
  # annotated tag lists its peeled (^{}) sha, the commit; a lightweight tag's
  # own sha already is the commit.
  local listing
  listing="$(git ls-remote --tags origin 'refs/tags/v*')" || return 1
  awk '
    $2 ~ /^refs\/tags\/v[0-9]+\.[0-9]+\.[0-9]+(\^\{\})?$/ {
      ver = substr($2, 12)
      peeled = (ver ~ /\^\{\}$/)
      sub(/\^\{\}$/, "", ver)
      if (peeled || !(ver in sha)) sha[ver] = $1
    }
    END { for (ver in sha) print ver, sha[ver] }
  ' <<<"$listing" | sort -rV
}

previous_release_tag() {
  # The newest vX.Y.Z tag on origin below the requested version $1 (no `v`), or
  # at it when $2 is "upto", whose commit is an ancestor of HEAD; prints the tag
  # WITH its `v`, nothing when none qualifies. Walks newest first and stops at
  # the first reachable one, so a candidate whose commit this clone lacks
  # refuses (reachability is unknowable) instead of being skipped, and so does
  # a reachable winner whose local tag is missing or names another commit (the
  # tool resolves the baseline by tag name). Fetching would move refs, so the
  # fix is only named, never run. Errors are written
  # here; errexit is off inside this function when callers test it, so every
  # status is handled by hand.
  local requested="$1" bound="${2:-below}" entries="" ver sha shallow="" status local_commit
  if shallow="$(git rev-parse --is-shallow-repository 2>/dev/null)" && [ "$shallow" = "true" ]; then
    echo "Error: this clone is shallow, so which tags are reachable from HEAD cannot be told; deepen it, then re-run:" >&2
    echo "  git fetch --unshallow --tags origin" >&2
    return 3
  fi
  if ! entries="$(origin_release_tags)"; then
    echo "Error: git ls-remote --tags origin failed; cannot determine the API check's baseline" >&2
    return 1
  fi
  while read -r ver sha; do
    [ -n "$ver" ] || continue
    if [ "$ver" = "$requested" ]; then
      [ "$bound" = "upto" ] || continue
    elif ! version_lt "$ver" "$requested"; then
      continue
    fi
    if ! git cat-file -e "${sha}^{commit}" 2>/dev/null; then
      echo "Error: origin's tag v${ver} points at a commit this clone does not have, so whether it is reachable from HEAD cannot be told; fetch the tags, then re-run:" >&2
      echo "  git fetch --tags origin" >&2
      return 3
    fi
    status=0
    git merge-base --is-ancestor "$sha" HEAD || status=$?
    case "$status" in
      0)
        # the tool resolves the baseline by tag name, so the local tag must
        # exist and name the commit that was just judged reachable
        local_commit=""
        if ! local_commit="$(git rev-parse --verify --quiet "refs/tags/v${ver}^{commit}")"; then
          echo "Error: origin's tag v${ver} is not a tag in this clone, and the API check resolves the baseline by tag name; fetch the tags, then re-run:" >&2
          echo "  git fetch --tags origin" >&2
          return 3
        fi
        if [ "$local_commit" != "$sha" ]; then
          echo "Error: this clone's tag v${ver} points at a different commit than origin's, and the API check resolves the baseline by tag name; take origin's, then re-run:" >&2
          echo "  git fetch --force --tags origin" >&2
          return 3
        fi
        printf 'v%s\n' "$ver"
        return 0
        ;;
      1) ;;
      *)
        echo "Error: git merge-base failed (exit ${status}) for tag v${ver}; cannot determine the API check's baseline" >&2
        return 1
        ;;
    esac
  done <<<"$entries"
  return 0
}

baseline_has_manifest() {
  # Forwards git grep's status for the tree of the tag $1 (e.g. v1.4.2): 0 when
  # a Cargo.toml there has SEMVER_PACKAGE as a package name, 1 when none does
  # (the tool looks the crate up by name, so a baseline predating the crate is
  # a terminal failure), anything else when git itself failed. A lockfile
  # naming the crate does not count.
  git grep -q --fixed-strings "name = \"${SEMVER_PACKAGE}\"" "${1}^{tree}" \
    -- 'Cargo.toml' '*/Cargo.toml'
}

resolve_baseline() {
  # The one baseline rule: prints the baseline tag (with its `v`) for the
  # requested version $1 under bound $2 (below|upto), or refuses with a message
  # and a status (see the list at the top).
  local requested="$1" bound="${2:-below}" tag="" status=0 manifest_status=0 relation="below"
  tag="$(previous_release_tag "$requested" "$bound")" || status=$?
  [ "$status" -eq 0 ] || return "$status"
  if [ -z "$tag" ]; then
    if [ "$bound" = "upto" ]; then relation="at or below"; fi
    echo "Error: no previous v* release tag on origin ${relation} v${requested} is reachable from HEAD; the API check has no baseline, and a release without one is refused" >&2
    return 4
  fi
  baseline_has_manifest "$tag" || manifest_status=$?
  case "$manifest_status" in
    0) ;;
    1)
      echo "Error: the baseline tag ${tag} has no ${SEMVER_PACKAGE} manifest (the crate did not exist at that tag); the API check cannot run against it" >&2
      return 5
      ;;
    *)
      echo "Error: could not read the tree of the baseline tag ${tag} (git grep exited ${manifest_status}); cannot tell whether it has a ${SEMVER_PACKAGE} manifest" >&2
      return 1
      ;;
  esac
  printf '%s\n' "$tag"
}

require_semver_toolchain() {
  # Refuses, naming the install command, when SEMVER_TOOLCHAIN is not
  # installed. `rustup run` installs nothing unless asked (rustup 1.29.2,
  # measured), where `cargo +<toolchain>` downloads a missing toolchain in the
  # middle of a check.
  rustup run "$SEMVER_TOOLCHAIN" rustc --version >/dev/null 2>&1 || {
    echo "Error: the ${SEMVER_PACKAGE} API check needs the ${SEMVER_TOOLCHAIN} toolchain, which is not installed (install: rustup toolchain install ${SEMVER_TOOLCHAIN})" >&2
    return 1
  }
}

semver_check() {
  # Run the check for baseline tag $1 (with its `v`) and release type $2;
  # forward the exit status (0 ok, 100 deny-level violation, 101 incomplete).
  cargo "+${SEMVER_TOOLCHAIN}" semver-checks check-release -p "$SEMVER_PACKAGE" \
    --baseline-rev "$1" --release-type "$2"
}

needed_release_type() {
  # The smallest release type (patch, minor, major) the tool accepts the
  # change as against the baseline tag $1; prints it. The first run keeps the
  # tool's output on stderr (the strictest type lists every finding), the
  # others are quiet. A run that cannot complete returns its status.
  local tag="$1" kind status
  for kind in patch minor major; do
    status=0
    if [ "$kind" = patch ]; then
      semver_check "$tag" "$kind" >&2 || status=$?
    else
      semver_check "$tag" "$kind" >/dev/null 2>&1 || status=$?
    fi
    case "$status" in
      0) echo "$kind"; return 0 ;;
      100) ;;
      *)
        echo "Error: the ${SEMVER_PACKAGE} API check could not complete (cargo-semver-checks exited ${status}) as a ${kind} release" >&2
        return "$status"
        ;;
    esac
  done
  echo "Error: the ${SEMVER_PACKAGE} API check denied the change even as a major release" >&2
  return 100
}

manifest_version() {
  # The version the root manifest declares: [workspace.package] first, then
  # [package].
  local version=""
  version="$(sed -nE '/^\[workspace\.package\]/,/^\[/{s/^version = "(.*)"/\1/p;}' Cargo.toml)"
  if [ -z "$version" ]; then
    version="$(sed -nE '/^\[package\]/,/^\[/{s/^version = "(.*)"/\1/p;}' Cargo.toml)"
  fi
  version="${version%%$'\n'*}"
  if [ -z "$version" ]; then
    echo "Error: no version found in Cargo.toml's [workspace.package] or [package]" >&2
    return 1
  fi
  printf '%s\n' "$version"
}

report_skip() {
  # A GitHub annotation on stdout, like the workflows' ::error:: lines, so a
  # job that passes without a verdict still shows why on its summary.
  echo "::warning::report-only: ${1}; no verdict (this job fails only when the manifest version is bumped to a type too small for the change)"
}

report() {
  # Report-only: print the release type the change needs against the newest
  # reachable origin tag at or below the manifest version. Exit 1 only when
  # the manifest is already bumped above that tag by a smaller release type
  # than the change needs; a manifest at the tag (not bumped yet) is reported
  # and passes, and anything that stops a verdict (a failed git command, no
  # baseline, a check that cannot complete) says so and passes.
  local cur baseline ver needed implied suggestion=""
  require_semver_toolchain || { report_skip "the ${SEMVER_TOOLCHAIN} toolchain is not installed"; return 0; }
  cur="$(manifest_version)" || { report_skip "the manifest version is unreadable"; return 0; }
  baseline="$(resolve_baseline "$cur" upto)" || { report_skip "no usable baseline tag"; return 0; }
  ver="${baseline#v}"
  needed="$(needed_release_type "$baseline")" || { report_skip "the check against ${baseline} could not finish"; return 0; }
  if [ "$ver" = "$cur" ]; then
    echo "${SEMVER_PACKAGE}: the change against ${baseline} needs $(release_label "$needed" "$ver"); the manifest is at ${cur}, not bumped yet"
    return 0
  fi
  implied="$(release_type "$ver" "$cur")"
  if [ "$(type_rank "$implied")" -lt "$(type_rank "$needed")" ]; then
    suggestion="$(needed_version "$ver" "$needed" 2>/dev/null)" || suggestion=""
    echo "Error: ${SEMVER_PACKAGE}: the change against ${baseline} needs $(release_label "$needed" "$ver"), but manifest version ${cur} is only $(release_label "$implied" "$ver")${suggestion:+; bump to at least ${suggestion}}" >&2
    return 1
  fi
  echo "${SEMVER_PACKAGE}: the change against ${baseline} needs $(release_label "$needed" "$ver"); manifest version ${cur} is $(release_label "$implied" "$ver"), which announces it"
}

if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
  case "${1:-}" in
    baseline)
      resolve_baseline "${2:?}" "${3:-below}"
      ;;
    release-type)
      release_type "${2:?}" "${3:?}"
      ;;
    check)
      require_semver_toolchain
      semver_check "${2:?}" "${3:?}"
      ;;
    manifest-version)
      manifest_version
      ;;
    report)
      report
      ;;
    *)
      echo "usage: semver-guard.sh baseline <version> [below|upto] | release-type <base> <cur> | check <base-tag> <type> | manifest-version | report" >&2
      exit 2
      ;;
  esac
fi
