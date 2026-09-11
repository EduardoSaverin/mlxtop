#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Package an existing release payload, including its dependency notices.
set -euo pipefail

if [[ $# != 2 ]]; then
    printf 'Usage: %s RELEASE_PAYLOAD OUTPUT_DIRECTORY\n' "$0" >&2
    exit 1
fi
payload="$(cd -- "$1" && pwd -P)"
mkdir -p "$2"
output="$(cd -- "$2" && pwd -P)"
version="$("$payload/mlxtop" --version)"
version="${version#mlxtop }"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 1
[[ -f "$payload/LICENSE" && -d "$payload/licenses" ]] || {
    printf '%s\n' 'Release payload must include LICENSE and dependency licenses.' >&2
    exit 1
}
work="$(mktemp -d "${TMPDIR:-/tmp}/mlxtop-dmg.XXXXXX")"
trap 'rm -rf "$work"' EXIT
root="$work/root"
resources="$work/resources"
image_root="$work/image"
mkdir -p "$root/usr/local/bin" "$root/usr/local/share/mlxtop/$version" "$resources" "$image_root"
cp "$payload/mlxtop" "$root/usr/local/bin/mlxtop"
chmod 755 "$root/usr/local/bin/mlxtop"
for path in "$payload"/*; do
    [[ "${path##*/}" == mlxtop ]] && continue
    cp -R "$path" "$root/usr/local/share/mlxtop/$version/"
done
cp "$payload/LICENSE" "$resources/LICENSE.txt"
cat > "$resources/Welcome.html" <<'HTML'
<html><body>
<h1>mlxtop</h1>
<p>A top for your local LLM on Mac.</p>
<p>This installer adds the mlxtop terminal command to /usr/local/bin and
installs documentation and license notices in /usr/local/share/mlxtop.</p>
<p>After installation, open Terminal and type <b>mlxtop</b>.</p>
<p>Requires macOS 11 or later on Apple Silicon. Installation requires an administrator account.</p>
</body></html>
HTML
cat > "$work/Distribution.xml" <<XML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
  <title>mlxtop $version</title>
  <welcome file="Welcome.html"/>
  <license file="LICENSE.txt"/>
  <options customize="never" require-scripts="false" hostArchitectures="arm64"/>
  <domains enable_anywhere="false" enable_currentUserHome="false" enable_localSystem="true"/>
  <volume-check><allowed-os-versions><os-version min="11.0"/></allowed-os-versions></volume-check>
  <choices-outline><line choice="default"/></choices-outline>
  <choice id="default" visible="false"><pkg-ref id="io.github.maximpri.mlxtop"/></choice>
  <pkg-ref id="io.github.maximpri.mlxtop" version="$version" onConclusion="none">mlxtop-component.pkg</pkg-ref>
</installer-gui-script>
XML
pkgbuild --root "$root" --identifier io.github.maximpri.mlxtop \
    --version "$version" --install-location / --ownership recommended "$work/mlxtop-component.pkg"
productbuild --distribution "$work/Distribution.xml" --resources "$resources" \
    --package-path "$work" "$image_root/Install mlxtop.pkg"
cat > "$image_root/READ ME.txt" <<TXT
mlxtop $version
A top for your local LLM on Mac.

1. Open Install mlxtop.pkg and follow the macOS Installer steps.
2. Open Terminal and run: mlxtop
3. Press q to quit.

The installer places the command at /usr/local/bin/mlxtop.
Documentation and licenses go in /usr/local/share/mlxtop/$version.
If your terminal cannot find mlxtop, run /usr/local/bin/mlxtop directly.
You can eject this disk image after installation.

Requires Apple Silicon and macOS 11 or later. Tested on macOS 26.5.1.
The package is unsigned and not Apple notarized.

Project and installation help: https://github.com/maximpri/mlxtop
TXT
dmg="mlxtop-${version}-aarch64-apple-darwin.dmg"
hdiutil create -volname "mlxtop $version" -srcfolder "$image_root" \
    -format UDZO -ov "$output/$dmg"
(cd "$output" && shasum -a 256 "$dmg" > SHA256SUMS)
printf 'Created %s/%s\n' "$output" "$dmg"
