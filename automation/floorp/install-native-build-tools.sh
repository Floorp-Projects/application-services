#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_PATH="${FLOORP_IOS_RELEASE_CONFIG:-${SCRIPT_DIR}/ios-xcframework-release-config.json}"

config_value() {
    python3 - "$CONFIG_PATH" "$1" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    value = json.load(stream)
for component in sys.argv[2].split("."):
    value = value[component]
print(value)
PY
}

verify_sha256() {
    python3 - "$1" "$2" <<'PY'
import hashlib
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
expected = sys.argv[2]
actual = hashlib.sha256(path.read_bytes()).hexdigest()
if actual != expected:
    raise SystemExit(f"SHA-256 mismatch for {path}: {actual} != {expected}")
PY
}

if [[ -z "${RUNNER_TEMP:-}" ]]; then
    echo "RUNNER_TEMP must be set" >&2
    exit 1
fi

ninja_version="$(config_value toolchain.ninja.version)"
gyp_commit="$(config_value toolchain.gyp.commit)"
six_wheel_url="$(config_value toolchain.gyp.six_wheel_url)"
six_wheel_sha="$(config_value toolchain.gyp.six_wheel_sha256)"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)
        ninja_archive="ninja-mac.zip"
        ninja_sha="$(config_value toolchain.ninja.macos_sha256)"
        ;;
    Linux-x86_64)
        ninja_archive="ninja-linux.zip"
        ninja_sha="$(config_value toolchain.ninja.linux_sha256)"
        ;;
    *)
        echo "Unsupported native build host: $(uname -s)-$(uname -m)" >&2
        exit 1
        ;;
esac

tools_root="${RUNNER_TEMP}/floorp-native-tools"
bin_dir="${tools_root}/bin"
gyp_dir="${tools_root}/gyp"
mkdir -p "$bin_dir"

ninja_zip="${tools_root}/${ninja_archive}"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "https://github.com/ninja-build/ninja/releases/download/v${ninja_version}/${ninja_archive}" \
    --output "$ninja_zip"
verify_sha256 "$ninja_zip" "$ninja_sha"
unzip -q -o "$ninja_zip" -d "$bin_dir"
chmod +x "${bin_dir}/ninja"

git init -q "$gyp_dir"
git -C "$gyp_dir" remote add origin https://chromium.googlesource.com/external/gyp.git
git -C "$gyp_dir" -c protocol.version=2 fetch --quiet --depth=1 origin "$gyp_commit"
git -C "$gyp_dir" checkout --quiet --detach FETCH_HEAD
actual_gyp_commit="$(git -C "$gyp_dir" rev-parse HEAD)"
if [[ "$actual_gyp_commit" != "$gyp_commit" ]]; then
    echo "Unexpected GYP commit: $actual_gyp_commit" >&2
    exit 1
fi

six_wheel="${tools_root}/six-1.17.0-py2.py3-none-any.whl"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "$six_wheel_url" \
    --output "$six_wheel"
verify_sha256 "$six_wheel" "$six_wheel_sha"

printf '#!/usr/bin/env bash\nexec env PYTHONPATH=%q python3 %q "$@"\n' \
    "${gyp_dir}/pylib:${six_wheel}" "${gyp_dir}/gyp_main.py" > "${bin_dir}/gyp"
chmod +x "${bin_dir}/gyp"
ln -sf "$(command -v python3)" "${bin_dir}/python"

"${bin_dir}/ninja" --version
"${bin_dir}/gyp" --help >/dev/null

if [[ -n "${GITHUB_PATH:-}" ]]; then
    echo "$bin_dir" >> "$GITHUB_PATH"
fi

echo "Installed pinned Ninja ${ninja_version} and GYP ${gyp_commit}."
