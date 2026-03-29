#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-}"

cd "$(dirname "${BASH_SOURCE[0]}")"

echo "Setting version ${VERSION}"
sed -i "s/^version = \".*\"/version = \"${VERSION}\"/" libplasmoid-updater/Cargo.toml
sed -i "s/^version = \".*\"/version = \"${VERSION}\"/" plasmoid-updater/Cargo.toml
sed -i "s/\(libplasmoid-updater = { path = \"..\/libplasmoid-updater\", version = \"\)[^\"]*\"/\1${VERSION}\"/" plasmoid-updater/Cargo.toml

echo "Updating Cargo.lock"
cargo check --workspace --quiet

echo "Committing version bump"
git add libplasmoid-updater/Cargo.toml plasmoid-updater/Cargo.toml Cargo.lock
git commit -m "chore: release v${VERSION}"

echo "Publishing libplasmoid-updater"
cargo publish -p libplasmoid-updater

echo "Waiting for crates.io index..."
sleep 10

echo "Publishing plasmoid-updater"
cargo publish -p plasmoid-updater

echo "Pushing main"
git push origin main

echo "Tagging and pushing v${VERSION}"
git tag "v${VERSION} -m release"
git push origin "v${VERSION}"

echo "Done: released v${VERSION}"
