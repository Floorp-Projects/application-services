#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CONFIG_PATH="${FLOORP_IOS_RELEASE_CONFIG:-${SCRIPT_DIR}/ios-xcframework-release-config.json}"

usage() {
    echo "Usage: $0 <output-directory> <release-tag> <source-commit> <repository> <workflow-url>" >&2
}

if [[ "$#" -ne 5 ]]; then
    usage
    exit 2
fi

output_dir="$1"
release_tag="$2"
source_commit="$3"
repository="$4"
workflow_url="$5"
upstream_ref="${FLOORP_IOS_UPSTREAM_REF:-}"

if [[ -z "$upstream_ref" ]]; then
    echo "FLOORP_IOS_UPSTREAM_REF must name the fetched upstream main ref" >&2
    exit 1
fi

require_runner_path() {
    local name="$1"
    local path="$2"
    if [[ -z "${RUNNER_TEMP:-}" ]]; then
        echo "RUNNER_TEMP must be set" >&2
        exit 1
    fi
    python3 - "$name" "$path" "$RUNNER_TEMP" <<'PY'
import pathlib
import sys

name, value, runner_temp = sys.argv[1:]
path = pathlib.Path(value).resolve()
root = pathlib.Path(runner_temp).resolve()
if path == root or root not in path.parents:
    raise SystemExit(f"{name} must be a dedicated path below RUNNER_TEMP: {value}")
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

require_runner_path CARGO_HOME "${CARGO_HOME:-}"
require_runner_path RUSTUP_HOME "${RUSTUP_HOME:-}"
require_runner_path output-directory "$output_dir"

cd "$REPO_ROOT"
python3 "$SCRIPT_DIR/verify-ios-xcframework-release.py" \
    --config "$CONFIG_PATH" \
    --release-tag "$release_tag" \
    --source-commit "$source_commit" \
    --upstream-ref "$upstream_ref" \
    --repository "$repository" \
    --source-only

expected_xcode="$(config_value toolchain.xcode)"
actual_xcode="$(xcodebuild -version | awk 'NR == 1 { print $2 }')"
if [[ "$actual_xcode" != "$expected_xcode" ]]; then
    echo "Expected Xcode ${expected_xcode}, found ${actual_xcode}" >&2
    exit 1
fi

expected_rust="$(config_value toolchain.rust)"
actual_rust="$(rustc --version | awk '{ print $2 }')"
if [[ "$actual_rust" != "$expected_rust" ]]; then
    echo "Expected Rust ${expected_rust}, found ${actual_rust}" >&2
    exit 1
fi

if [[ -e "$output_dir" ]] && [[ -n "$(find "$output_dir" -mindepth 1 -print -quit)" ]]; then
    echo "Output directory must be empty: $output_dir" >&2
    exit 1
fi
mkdir -p "$output_dir"

work_dir="${RUNNER_TEMP}/floorp-ios-xcframework-work"
if [[ -e "$work_dir" ]]; then
    echo "Working directory already exists: $work_dir" >&2
    exit 1
fi
mkdir -p "$work_dir"

export CARGO="${CARGO_HOME}/bin/cargo"
IOS_DEPLOYMENT_TARGET="$(config_value ios.deployment_target)"
export IOS_DEPLOYMENT_TARGET
export NSS_STATIC=1
SOURCE_DATE_EPOCH="$(git show -s --format=%ct "$source_commit")"
export SOURCE_DATE_EPOCH
export GLEAN_BUILD_DATE=0
export GLEAN_PARSER_REQUIREMENTS_FILE="${SCRIPT_DIR}/glean-parser-requirements.txt"
export PIP_DISABLE_PIP_VERSION_CHECK=1
export PIP_NO_INPUT=1
export PIP_CACHE_DIR="${RUNNER_TEMP}/floorp-pip-cache"

"$CARGO" fetch --locked
for target in x86_64-apple-ios aarch64-apple-ios aarch64-apple-ios-sim; do
    "$CARGO" fetch --locked --target "$target"
done
export CARGO_NET_OFFLINE=true

pushd libs >/dev/null
./build-all.sh ios
popd >/dev/null
for nss_dir in \
    libs/ios/x86_64/nss \
    libs/ios/arm64/nss \
    libs/ios/arm64-sim/nss \
    libs/ios/universal/nss; do
    if [[ ! -d "$nss_dir" ]]; then
        echo "NSS build output is missing: $nss_dir" >&2
        exit 1
    fi
done

python3 taskcluster/scripts/build-and-test-swift.py \
    "$work_dir/swift-components" \
    "$output_dir" \
    "$work_dir/glean-workdir" \
    --force_build

archive_timestamp="$(date -u -r "$SOURCE_DATE_EPOCH" '+%Y%m%d%H%M.%S')"
find "$work_dir/swift-components" -exec touch -h -t "$archive_timestamp" {} +
COPYFILE_DISABLE=1 tar -C "$work_dir" -cJf "$output_dir/swift-components.tar.xz" swift-components

git diff --exit-code -- Cargo.lock

python3 "$SCRIPT_DIR/verify-ios-xcframework-release.py" \
    --config "$CONFIG_PATH" \
    --release-tag "$release_tag" \
    --source-commit "$source_commit" \
    --upstream-ref "$upstream_ref" \
    --repository "$repository" \
    --workflow-url "$workflow_url" \
    --artifacts "$output_dir" \
    --write-metadata

echo "Verified Floorp iOS release artifacts in $output_dir"
