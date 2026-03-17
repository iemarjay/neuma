#!/usr/bin/env bash
# Usage: ./scripts/release.sh [patch|minor|major]  (default: patch)
set -euo pipefail

BUMP="${1:-patch}"

# ── 1. Read current version from Cargo.toml ─────────────────────────────────
CURRENT=$(grep '^version = ' src-tauri/Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

case "$BUMP" in
  major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
  patch) PATCH=$((PATCH + 1)) ;;
  *) echo "Usage: $0 [patch|minor|major]"; exit 1 ;;
esac

VERSION="$MAJOR.$MINOR.$PATCH"
TAG="v$VERSION"

echo "$CURRENT → $VERSION"

# ── 2. Bump version in Cargo.toml and tauri.conf.json ───────────────────────
sed -i '' "s/^version = \"$CURRENT\"/version = \"$VERSION\"/" src-tauri/Cargo.toml
sed -i '' "s/\"version\": \"$CURRENT\"/\"version\": \"$VERSION\"/" src-tauri/tauri.conf.json

# ── 3. Commit + tag ──────────────────────────────────────────────────────────
git add src-tauri/Cargo.toml src-tauri/tauri.conf.json
git commit -m "chore: release $TAG"
git tag "$TAG"

echo "Tagged $TAG — pushing..."
git push origin main "$TAG"

# ── 4. Build locally ─────────────────────────────────────────────────────────
echo "Building..."
npm run build
cargo tauri build

# ── 5. Upload .dmg to the draft release ──────────────────────────────────────
DMG=$(ls src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1)

if [[ -z "$DMG" ]]; then
  echo "ERROR: no .dmg found in src-tauri/target/release/bundle/dmg/"
  exit 1
fi

echo "Uploading $DMG..."
gh release upload "$TAG" "$DMG"

echo ""
echo "macOS uploaded. Windows is building on GitHub Actions."
echo "Publish when ready: gh release edit $TAG --draft=false"
echo "Or view: $(gh release view "$TAG" --json url -q .url)"
