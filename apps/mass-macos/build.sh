#!/usr/bin/env bash
# Builds the Finder host app and its embedded extension for this Mac.
# An unsigned build is suitable for compiler and packaging checks only.
set -euo pipefail

if [[ "$#" != 3 || ( "$1" != --unsigned && "$1" != --signed ) ]]; then
  printf 'Usage: bash apps/mass-macos/build.sh --signed|--unsigned OUTPUT.app MASS_DEVICE_BINARY\n' >&2
  exit 2
fi
mode="$1"
output="$2"
helper="$3"
root="$(cd "$(dirname "$0")" && pwd -P)"
if [[ "$output" != *.app || ! -d "$(dirname "$output")" || -e "$output" || -L "$output" || ! -x "$helper" ]]; then
  printf 'Select a new .app path with an existing parent and a built mass-device executable.\n' >&2
  exit 2
fi

bundle_id="${MASS_BUNDLE_ID:-com.jumpstart.mass.finder}"
writes="${MASS_ENABLE_WRITE_PREVIEW:-0}"
[[ "$writes" == 0 || "$writes" == 1 ]]
team="UNSIGNED"
prefix="UNSIGNED"
if [[ "$mode" == --signed ]]; then
  : "${MASS_SIGNING_IDENTITY:?Set the installed Apple signing identity}"
  : "${MASS_TEAM_ID:?Set the Apple development team ID}"
  : "${MASS_APP_PROFILE:?Supply the host app provisioning profile}"
  : "${MASS_PROVIDER_PROFILE:?Supply the File Provider provisioning profile}"
  [[ "$MASS_SIGNING_IDENTITY" != "-" && -f "$MASS_APP_PROFILE" && -f "$MASS_PROVIDER_PROFILE" ]]
  team="$MASS_TEAM_ID"
  prefix="${MASS_APP_IDENTIFIER_PREFIX:-$team}"
fi
[[ "$bundle_id" =~ ^[a-zA-Z0-9-]+(\.[a-zA-Z0-9-]+)+$ && "$team" =~ ^[A-Z0-9]+$ && "$prefix" =~ ^[A-Z0-9]+$ ]]
group="$prefix.$bundle_id.shared"

stage="$(mktemp -d "$(dirname "$output")/.mass-build.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT
app="$stage/Mass Finder.app"
provider="$app/Contents/PlugIns/MassFinderExtension.appex"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers" "$provider/Contents/MacOS" "$provider/Contents/Helpers"
cache="${MASS_SWIFT_MODULE_CACHE:-$stage/module-cache}"
mkdir -p "$cache"
flags=(-swift-version 5 -warnings-as-errors -module-cache-path "$cache" -Xcc "-fmodules-cache-path=$cache" -target "$(uname -m)-apple-macosx14.0")
swiftc "${flags[@]}" -parse-as-library -module-name MassFinder \
  "$root/Shared/MassConnection.swift" "$root/App/MassFinderApp.swift" \
  -o "$app/Contents/MacOS/MassFinder"
swiftc "${flags[@]}" -parse-as-library -application-extension -module-name MassFinderExtension \
  -Xlinker -e -Xlinker _NSExtensionMain \
  "$root/Shared/MassConnection.swift" "$root/Provider/MountNames.swift" "$root/Provider/WriteJournal.swift" "$root/Provider/MassProvider.swift" \
  -o "$provider/Contents/MacOS/MassFinderExtension"

cp "$helper" "$app/Contents/Helpers/mass-device"
cp "$helper" "$provider/Contents/Helpers/mass-device"
cp "$root/App/Info.plist" "$app/Contents/Info.plist"
cp "$root/Provider/Info.plist" "$provider/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "$bundle_id" "$app/Contents/Info.plist"
plutil -replace CFBundleIdentifier -string "$bundle_id.provider" "$provider/Contents/Info.plist"
for item in "$app" "$provider"; do
  plutil -replace MassKeychainGroup -string "$group" "$item/Contents/Info.plist"
  if [[ "$writes" == 1 ]]; then
    plutil -insert MassWritePreview -bool true "$item/Contents/Info.plist"
  fi
  plutil -lint "$item/Contents/Info.plist"
done

if [[ "$mode" == --signed ]]; then
  cp "$MASS_APP_PROFILE" "$app/Contents/embedded.provisionprofile"
  cp "$MASS_PROVIDER_PROFILE" "$provider/Contents/embedded.provisionprofile"
  for item in "$app" "$provider"; do
    id="$(plutil -extract CFBundleIdentifier raw "$item/Contents/Info.plist")"
    entitlements="$stage/$id.entitlements"
    cp "$root/Shared/Native.entitlements" "$entitlements"
    /usr/libexec/PlistBuddy -c "Set :com.apple.application-identifier $prefix.$id" "$entitlements"
    /usr/libexec/PlistBuddy -c "Set :com.apple.developer.team-identifier $team" "$entitlements"
    /usr/libexec/PlistBuddy -c "Set :keychain-access-groups:0 $group" "$entitlements"
    codesign --force --sign "$MASS_SIGNING_IDENTITY" --options runtime \
      --entitlements "$root/Shared/Helper.entitlements" "$item/Contents/Helpers/mass-device"
    # Sign the embedded extension before sealing its enclosing app.
    if [[ "$item" == "$provider" ]]; then
      codesign --force --sign "$MASS_SIGNING_IDENTITY" --options runtime --entitlements "$entitlements" "$item"
    fi
  done
  codesign --force --sign "$MASS_SIGNING_IDENTITY" --options runtime \
    --entitlements "$stage/$bundle_id.entitlements" "$app"
  codesign --verify --deep --strict "$app"
fi

mkdir "$output"
if ! mv "$app/Contents" "$output/Contents"; then
  rmdir "$output"
  exit 1
fi
printf '%s\n' "$output"
