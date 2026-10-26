#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Developer-only arm64 simulator artifact. This is not a release build and must
# never be substituted for immutable, signed release provenance.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 /absolute/new-output-directory" >&2
  exit 2
fi
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd "$script_dir/../.." && pwd)"
output_dir="$1"
if [[ "$output_dir" != /* || -e "$output_dir" ]]; then
  echo "Output must be a new absolute directory (existing output is never overwritten)." >&2
  exit 2
fi
nss_dir="${FLOORP_SIMULATOR_NSS_DIR:-$repo_dir/libs/ios/arm64-sim/nss}"
if [[ ! -d "$nss_dir/lib" ]]; then
  echo "Build arm64 simulator NSS first: missing $nss_dir/lib" >&2
  exit 1
fi
mkdir -p "$output_dir"
cd "$repo_dir"
export IPHONEOS_DEPLOYMENT_TARGET="${IOS_DEPLOYMENT_TARGET:-15.0}"
# Per-command NSS environment prevents the simulator NSS from contaminating
# subsequent host bindgen builds. Cargo's host tools use their ordinary setup.
NSS_STATIC=1 NSS_DIR="$nss_dir" \
  cargo build --locked -p megazord_ios --target aarch64-apple-ios-sim
library="${CARGO_TARGET_DIR:-$repo_dir/target}/aarch64-apple-ios-sim/debug/libmegazord_ios.a"
framework_root="$output_dir/MozillaRustComponents.xcframework"
framework="$framework_root/ios-arm64-simulator/MozillaRustComponents.framework"
mkdir -p "$framework/Headers" "$framework/Modules" "$output_dir/Generated"
cp "$library" "$framework/MozillaRustComponents"
cp "$repo_dir/megazords/ios-rust/MozillaRustComponents.h" "$framework/Headers/"
cp "$repo_dir/megazords/ios-rust/Sources/MozillaRustComponentsWrapper/Viaduct/RustViaductFFI.h" "$framework/Headers/"
cargo run --locked -p uniffi-bindgen-library-mode -- -l "$library" swift "$framework/Headers" --headers
cargo run --locked -p uniffi-bindgen-library-mode -- -l "$library" swift "$framework/Modules" --modulemap --xcframework --modulemap-filename module.modulemap
cargo run --locked -p uniffi-bindgen-library-mode -- -l "$library" swift "$output_dir/Generated" --swift-sources
python3 - "$repo_dir" "$output_dir" "$library" <<'PY'
import hashlib, json, pathlib, plistlib, subprocess, sys
repo, output, library = map(pathlib.Path, sys.argv[1:])
framework_root = output / "MozillaRustComponents.xcframework"
framework = framework_root / "ios-arm64-simulator/MozillaRustComponents.framework"
modulemap = framework / "Modules/module.modulemap"
lines = modulemap.read_text().splitlines()
lines.insert(1, '    header "RustViaductFFI.h"')
modulemap.write_text("\n".join(lines) + "\n")
for name in ("glean_sym.swift", "glean_sym.h"):
    (output / "Generated" / name).unlink(missing_ok=True)
required = {
    "ffi_floorp_workspaces_sync_uniffi_contract_version",
    "ffi_floorp_prefs_sync_uniffi_contract_version",
}
symbols = subprocess.check_output(["nm", "-gU", str(library)], text=True, stderr=subprocess.DEVNULL)
if any(symbol not in symbols for symbol in required):
    raise SystemExit("The simulator library is missing Floorp UniFFI metadata")
for stem in ("floorp_workspaces_sync", "floorp_prefs_sync"):
    if not (output / "Generated" / f"{stem}.swift").is_file():
        raise SystemExit(f"Missing generated Swift binding: {stem}")
    header = framework / "Headers" / f"{stem}FFI.h"
    if not header.is_file():
        raise SystemExit(f"Missing generated FFI header: {stem}")
plist = {
    "CFBundlePackageType": "XFWK", "XCFrameworkFormatVersion": "1.0",
    "AvailableLibraries": [{
        "LibraryIdentifier": "ios-arm64-simulator",
        "LibraryPath": "MozillaRustComponents.framework",
        "SupportedArchitectures": ["arm64"],
        "SupportedPlatform": "ios", "SupportedPlatformVariant": "simulator",
    }],
}
(framework_root / "Info.plist").write_bytes(plistlib.dumps(plist))
files = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=repo).split(b"\0")
source_hash = hashlib.sha256()
for raw in sorted(set(files)):
    if not raw:
        continue
    path = repo / raw.decode()
    if not path.is_file():
        continue
    source_hash.update(raw + b"\0" + path.read_bytes() + b"\0")
manifest = {
    "schemaVersion": 1, "kind": "developer-simulator-only", "releaseEligible": False,
    "target": "aarch64-apple-ios-sim", "profile": "debug",
    "sourceHead": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
    "sourceDirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=repo)),
    "sourceTreeSha256": source_hash.hexdigest(),
    "librarySha256": hashlib.sha256(library.read_bytes()).hexdigest(),
    "xcode": subprocess.check_output(["xcodebuild", "-version"], text=True).strip(),
    "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "warning": "Local developer artifact; device/Intel simulator and production release gates have not been validated.",
}
(output / "developer-build.json").write_text(json.dumps(manifest, indent=2) + "\n")
PY
printf 'Developer simulator framework: %s\nGenerated bindings: %s\n' "$framework_root" "$output_dir/Generated"
