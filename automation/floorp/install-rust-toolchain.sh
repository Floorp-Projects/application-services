#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_PATH="${FLOORP_IOS_RELEASE_CONFIG:-${SCRIPT_DIR}/ios-xcframework-release-config.json}"

require_temp_home() {
    local name="$1"
    local value="${!name:-}"

    if [[ -z "${RUNNER_TEMP:-}" || -z "$value" ]]; then
        echo "RUNNER_TEMP and $name must be set" >&2
        exit 1
    fi
    python3 - "$name" "$value" "$RUNNER_TEMP" <<'PY'
import pathlib
import sys

name, value, runner_temp = sys.argv[1:]
path = pathlib.Path(value).resolve()
root = pathlib.Path(runner_temp).resolve()
if path == root or root not in path.parents:
    raise SystemExit(f"{name} must be a dedicated directory below RUNNER_TEMP")
PY
}

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

require_temp_home CARGO_HOME
require_temp_home RUSTUP_HOME

rust_version="$(config_value toolchain.rust)"
rustup_version="$(config_value toolchain.rustup_init.version)"

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)
        rustup_platform="aarch64-apple-darwin"
        rustup_sha="$(config_value toolchain.rustup_init.aarch64_apple_darwin_sha256)"
        rust_targets=(aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
        ;;
    Linux-x86_64)
        rustup_platform="x86_64-unknown-linux-gnu"
        rustup_sha="$(config_value toolchain.rustup_init.x86_64_unknown_linux_gnu_sha256)"
        rust_targets=()
        ;;
    *)
        echo "Unsupported release build host: $(uname -s)-$(uname -m)" >&2
        exit 1
        ;;
esac

mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"
installer_dir="${RUNNER_TEMP}/floorp-rustup-installer"
mkdir -p "$installer_dir"
installer="${installer_dir}/rustup-init"
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    "https://static.rust-lang.org/rustup/archive/${rustup_version}/${rustup_platform}/rustup-init" \
    --output "$installer"
verify_sha256 "$installer" "$rustup_sha"
chmod +x "$installer"
"$installer" -y --no-modify-path --profile minimal --default-toolchain none

rustup_bin="${CARGO_HOME}/bin/rustup"
"$rustup_bin" set auto-self-update disable
install_args=(
    toolchain install "$rust_version"
    --no-self-update
    --profile minimal
    --component "clippy,llvm-tools-preview,rustfmt,rust-src"
)
if (( ${#rust_targets[@]} > 0 )); then
    install_args+=(--target "$(IFS=,; echo "${rust_targets[*]}")")
fi
"$rustup_bin" "${install_args[@]}"
"$rustup_bin" default "$rust_version"

export PATH="${CARGO_HOME}/bin:${PATH}"
export RUSTUP_TOOLCHAIN="$rust_version"
rustc_version="$(rustc --version)"
if [[ "$rustc_version" != "rustc ${rust_version} "* ]]; then
    echo "Unexpected Rust compiler: $rustc_version" >&2
    exit 1
fi

if [[ -n "${GITHUB_PATH:-}" ]]; then
    echo "${CARGO_HOME}/bin" >> "$GITHUB_PATH"
fi
if [[ -n "${GITHUB_ENV:-}" ]]; then
    echo "RUSTUP_TOOLCHAIN=${rust_version}" >> "$GITHUB_ENV"
fi

echo "Installed ${rustc_version} with isolated Rust homes."
