#!/usr/bin/env python3

"""Validate and describe Floorp's distributable iOS Application Services build."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import plistlib
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unicodedata
import zipfile
from typing import Any


SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
DEFAULT_CONFIG = SCRIPT_DIR / "ios-xcframework-release-config.json"
MAIN_ARCHIVE = "MozillaRustComponents.xcframework.zip"
FOCUS_ARCHIVE = "FocusRustComponents.xcframework.zip"
SWIFT_ARCHIVE = "swift-components.tar.xz"
MANIFEST = "release-manifest.json"
CHECKSUMS = "SHA256SUMS"
FLOORP_CONTRACT_SYMBOL = "ffi_floorp_prefs_sync_uniffi_contract_version"
APPLE_PLATFORM_NAMES = {
    "2": "IOS",
    "7": "IOSSIMULATOR",
    "IOS": "IOS",
    "IOSSIMULATOR": "IOSSIMULATOR",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=pathlib.Path, default=DEFAULT_CONFIG)
    parser.add_argument("--release-tag", required=True)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--upstream-ref", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--workflow-url", default="")
    parser.add_argument("--artifacts", type=pathlib.Path)
    parser.add_argument("--write-metadata", action="store_true")
    parser.add_argument("--source-only", action="store_true")
    args = parser.parse_args()
    if args.source_only and args.artifacts:
        parser.error("--source-only and --artifacts are mutually exclusive")
    if args.write_metadata and not args.artifacts:
        parser.error("--write-metadata requires --artifacts")
    if not args.source_only and not args.artifacts:
        parser.error("provide --source-only or --artifacts")
    return args


def load_config(path: pathlib.Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as stream:
        config = json.load(stream)
    if config.get("schema_version") != 1:
        raise ValueError("unsupported release config schema")
    if config.get("immutable_releases_required") is not True:
        raise ValueError("immutable releases must be required")
    expected_artifacts = [MAIN_ARCHIVE, FOCUS_ARCHIVE, SWIFT_ARCHIVE]
    if config.get("artifacts") != expected_artifacts:
        raise ValueError(f"artifacts must be exactly {expected_artifacts}")
    return config


def run(*command: str, cwd: pathlib.Path = REPO_ROOT) -> str:
    return subprocess.check_output(command, cwd=cwd, text=True).strip()


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def normalize_distribution_name(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def parse_hashed_requirements(path: pathlib.Path) -> dict[str, dict[str, Any]]:
    """Parse the intentionally small, hash-locked release requirements file."""
    logical_lines: list[str] = []
    pending = ""
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.endswith("\\"):
            pending += line[:-1].strip() + " "
            continue
        logical_lines.append((pending + line).strip())
        pending = ""
    if pending:
        raise ValueError("requirements file ends with an incomplete continuation")

    requirement_pattern = re.compile(
        r"(?P<name>[A-Za-z0-9_.-]+)==(?P<version>[^\s]+)"
        r"(?P<hashes>(?:\s+--hash=sha256:[0-9a-f]{64})+)"
    )
    parsed: dict[str, dict[str, Any]] = {}
    for line in logical_lines:
        match = requirement_pattern.fullmatch(line)
        if not match:
            raise ValueError(
                f"release requirement is not exactly pinned and hashed: {line}"
            )
        name = normalize_distribution_name(match.group("name"))
        if name in parsed:
            raise ValueError(f"duplicate release requirement: {name}")
        hashes = re.findall(r"--hash=sha256:([0-9a-f]{64})", match.group("hashes"))
        parsed[name] = {"version": match.group("version"), "hashes": hashes}
    if not parsed:
        raise ValueError("release requirements file is empty")
    return parsed


def parse_nss_build_configuration(text: str) -> dict[str, str]:
    """Parse the immutable NSS assignments used by libs/build-all.sh."""

    names = ("NSS", "NSS_ARCHIVE", "NSS_SHA256")
    lines = text.splitlines()
    markers = [
        index for index, line in enumerate(lines) if line == "# End of configuration."
    ]
    if len(markers) != 1:
        raise ValueError("NSS build configuration marker must appear exactly once")
    marker = markers[0]
    assignment = re.compile(
        r"^\s*(?:(?:export|readonly)\s+|declare(?:\s+-[A-Za-z]+)?\s+)?"
        r"(?P<name>NSS|NSS_ARCHIVE|NSS_SHA256)\s*\+?="
    )
    literal_assignment = re.compile(
        r'^(?P<name>NSS|NSS_ARCHIVE|NSS_SHA256)="(?P<value>[^"$`]*)"$'
    )
    values: dict[str, str] = {}
    for index, line in enumerate(lines):
        if not assignment.match(line):
            continue
        literal = literal_assignment.fullmatch(line)
        if index >= marker or literal is None:
            raise ValueError(f"unexpected active NSS assignment: {line!r}")
        name = literal.group("name")
        if name in values:
            raise ValueError(f"duplicate NSS build assignment: {name}")
        values[name] = literal.group("value")

    missing = sorted(set(names) - values.keys())
    if missing:
        raise ValueError(f"missing NSS build assignments: {missing}")
    readonly_line = "readonly NSS NSS_ARCHIVE NSS_URL NSS_SHA256"
    if lines[:marker].count(readonly_line) != 1:
        raise ValueError("NSS build variables must be made readonly before use")
    return values


def validate_upstream_base(
    upstream_commit: str,
    source_commit: str,
    upstream_ref: str,
    repo_root: pathlib.Path = REPO_ROOT,
) -> str:
    if not re.fullmatch(r"[0-9a-f]{40}", upstream_commit):
        raise ValueError("configured upstream commit must be a full lowercase Git SHA")
    ref_check = subprocess.run(
        ["git", "check-ref-format", upstream_ref],
        cwd=repo_root,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if ref_check.returncode != 0:
        raise ValueError(f"invalid upstream ref: {upstream_ref!r}")
    subprocess.run(
        ["git", "cat-file", "-e", f"{upstream_commit}^{{commit}}"],
        cwd=repo_root,
        check=True,
    )
    subprocess.run(
        ["git", "cat-file", "-e", f"{upstream_ref}^{{commit}}"],
        cwd=repo_root,
        check=True,
    )
    merge_bases = subprocess.check_output(
        ["git", "merge-base", "--all", source_commit, upstream_ref],
        cwd=repo_root,
        text=True,
    ).splitlines()
    if merge_bases != [upstream_commit]:
        raise ValueError(
            f"source and {upstream_ref} merge at {merge_bases}; "
            f"expected exactly {upstream_commit}"
        )
    return merge_bases[0]


def validate_source(
    config: dict[str, Any],
    release_tag: str,
    source_commit: str,
    upstream_ref: str,
    repository: str,
) -> str:
    if not re.fullmatch(config["release_tag_pattern"], release_tag):
        raise ValueError(
            f"release tag {release_tag!r} does not match "
            f"{config['release_tag_pattern']!r}"
        )
    if not re.fullmatch(r"[0-9a-f]{40}", source_commit):
        raise ValueError("source commit must be a full lowercase Git SHA")
    if repository != config["distribution_repository"]:
        raise ValueError(
            f"release repository {repository!r} is not "
            f"{config['distribution_repository']!r}"
        )

    actual_commit = run("git", "rev-parse", "HEAD")
    if actual_commit != source_commit:
        raise ValueError(f"HEAD {actual_commit} is not source commit {source_commit}")

    actual_merge_base = validate_upstream_base(
        config["upstream"]["commit"], source_commit, upstream_ref
    )

    source_version = (REPO_ROOT / "version.txt").read_text(encoding="utf-8").strip()
    if source_version != config["upstream"]["source_version"]:
        raise ValueError("version.txt and configured upstream source version differ")

    rust_toolchain = (REPO_ROOT / "rust-toolchain.toml").read_text(encoding="utf-8")
    rust_version = re.search(r'^channel = "([^"]+)"$', rust_toolchain, re.MULTILINE)
    if not rust_version or rust_version.group(1) != config["toolchain"]["rust"]:
        raise ValueError("rust-toolchain.toml and release config differ")

    build_all = (REPO_ROOT / "libs/build-all.sh").read_text(encoding="utf-8")
    actual_nss = parse_nss_build_configuration(build_all)
    expected_nss = {
        "NSS": f'nss-{config["nss"]["version"]}',
        "NSS_ARCHIVE": config["nss"]["archive"],
        "NSS_SHA256": config["nss"]["sha256"],
    }
    if actual_nss != expected_nss:
        raise ValueError(
            f"NSS build configuration {actual_nss!r} differs from {expected_nss!r}"
        )

    requirements = parse_hashed_requirements(
        SCRIPT_DIR / "glean-parser-requirements.txt"
    )
    glean_requirement = requirements.get("glean-parser")
    if not glean_requirement or (
        glean_requirement["version"] != config["toolchain"]["glean_parser"]
    ):
        raise ValueError("Glean Parser hash lock and release config differ")

    umbrella = (
        REPO_ROOT / "megazords/ios-rust/MozillaRustComponents.h"
    ).read_text(encoding="utf-8")
    if '#import "floorp_prefs_syncFFI.h"' not in umbrella:
        raise ValueError("Floorp preferences FFI is absent from the iOS umbrella header")
    return actual_merge_base


def safe_archive_name(name: str) -> bool:
    path = pathlib.PurePosixPath(name)
    return (
        bool(path.parts)
        and not path.is_absolute()
        and ".." not in path.parts
        and name.rstrip("/") == path.as_posix()
    )


def normalized_archive_name(name: str) -> str:
    canonical = pathlib.PurePosixPath(name.rstrip("/")).as_posix()
    return unicodedata.normalize("NFC", canonical).casefold()


def validate_zip_members(archive: zipfile.ZipFile) -> set[str]:
    members = archive.infolist()
    member_names = [member.filename for member in members]
    names = set(member_names)
    if len(names) != len(member_names):
        raise ValueError("ZIP archive contains duplicate member names")
    normalized_names = [normalized_archive_name(name) for name in member_names]
    if len(set(normalized_names)) != len(normalized_names):
        raise ValueError("ZIP archive contains colliding member names")
    unsafe = sorted(name for name in names if not safe_archive_name(name))
    if unsafe:
        raise ValueError(f"unsafe ZIP members: {unsafe[:3]}")
    unsupported: list[str] = []
    for member in members:
        unix_mode = member.external_attr >> 16
        file_type = stat.S_IFMT(unix_mode)
        if member.flag_bits & 0x1:
            unsupported.append(member.filename)
        elif member.is_dir():
            if file_type not in {0, stat.S_IFDIR}:
                unsupported.append(member.filename)
        elif file_type not in {0, stat.S_IFREG}:
            unsupported.append(member.filename)
    if unsupported:
        raise ValueError(
            f"ZIP archive contains links or special files: {unsupported[:3]}"
        )
    return names


def parse_macho_build_versions(output: str) -> list[dict[str, str]]:
    records: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if line == "cmd LC_BUILD_VERSION":
            if current:
                raise ValueError("incomplete LC_BUILD_VERSION metadata")
            current = {}
            continue
        if current is None:
            continue
        if line.startswith("platform "):
            platform = line.split(None, 1)[1].upper().replace("_", "")
            current["platform"] = APPLE_PLATFORM_NAMES.get(platform, platform)
        elif line.startswith("minos "):
            current["minos"] = line.split(None, 1)[1]
        elif line.startswith("cmd ") or line.startswith("Load command "):
            raise ValueError("incomplete LC_BUILD_VERSION metadata")
        if current.keys() >= {"platform", "minos"}:
            records.append(current)
            current = None
    if current:
        raise ValueError("incomplete LC_BUILD_VERSION metadata")
    return records


def apple_version(value: str) -> tuple[int, ...]:
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){0,2}", value):
        raise ValueError(f"invalid Apple platform version: {value!r}")
    return tuple(int(component) for component in value.split("."))


def validate_build_versions(
    records: list[dict[str, str]], expected_platform: str, deployment_target: str
) -> None:
    if not records:
        raise ValueError("binary has no LC_BUILD_VERSION metadata")
    maximum_target = apple_version(deployment_target)
    for record in records:
        if record["platform"] != expected_platform:
            raise ValueError(
                f"binary targets {record['platform']}; expected {expected_platform}"
            )
        if apple_version(record["minos"]) > maximum_target:
            raise ValueError(
                f"binary requires {record['minos']}; release supports iOS "
                f"{deployment_target}"
            )


def inspect_binary(binary: bytes, runner_temp: str | None) -> dict[str, Any]:
    temp_parent = pathlib.Path(runner_temp) if runner_temp else None
    with tempfile.TemporaryDirectory(dir=temp_parent) as directory:
        binary_path = pathlib.Path(directory) / "MozillaRustComponents"
        binary_path.write_bytes(binary)
        architectures = set(
            run("/usr/bin/lipo", "-archs", str(binary_path), cwd=REPO_ROOT).split()
        )
        build_versions = parse_macho_build_versions(
            run(
                "/usr/bin/otool",
                "-arch",
                "all",
                "-l",
                str(binary_path),
                cwd=REPO_ROOT,
            )
        )
        symbols_by_architecture: dict[str, set[str]] = {}
        for architecture in sorted(architectures):
            symbol_output = run(
                "/usr/bin/nm",
                "-arch",
                architecture,
                "-g",
                "-j",
                "-U",
                str(binary_path),
                cwd=REPO_ROOT,
            )
            symbols_by_architecture[architecture] = {
                line.strip().split()[-1].removeprefix("_")
                for line in symbol_output.splitlines()
                if line.strip() and not line.rstrip().endswith(":")
            }
    return {
        "architectures": architectures,
        "build_versions": build_versions,
        "symbols_by_architecture": symbols_by_architecture,
    }


def validate_modulemap(modulemap: str) -> None:
    if not re.search(
        r"\b(?:framework\s+)?module\s+MozillaRustComponents\b", modulemap
    ):
        raise ValueError("modulemap does not define MozillaRustComponents")
    if not re.search(
        r'\bumbrella\s+header\s+"MozillaRustComponents\.h"', modulemap
    ):
        raise ValueError("modulemap does not use MozillaRustComponents.h")


def validate_swift_imports(
    archive: zipfile.ZipFile,
    root_name: str,
    libraries: list[dict[str, Any]],
    deployment_target: str,
    required_symbol: str | None,
    runner_temp: str | None,
) -> list[str]:
    temp_parent = pathlib.Path(runner_temp) if runner_temp else None
    with tempfile.TemporaryDirectory(dir=temp_parent) as directory:
        extraction_root = pathlib.Path(directory)
        archive.extractall(extraction_root)
        source_path = extraction_root / "FloorpReleaseImportSmoke.swift"
        source = "import MozillaRustComponents\n"
        if required_symbol:
            source += f"func floorpReleaseSymbolSmoke() {{ _ = {required_symbol}() }}\n"
        source_path.write_text(source, encoding="utf-8")
        checked_targets: list[str] = []
        for library in libraries:
            if library["variant"] == "simulator":
                sdk = "iphonesimulator"
                targets = [
                    f"{architecture}-apple-ios{deployment_target}-simulator"
                    for architecture in library["architectures"]
                ]
            else:
                sdk = "iphoneos"
                targets = [
                    f"{architecture}-apple-ios{deployment_target}"
                    for architecture in library["architectures"]
                ]
            sdk_path = run("/usr/bin/xcrun", "--sdk", sdk, "--show-sdk-path")
            framework_search_path = (
                extraction_root / root_name / library["library_identifier"]
            )
            for target in targets:
                object_path = extraction_root / (
                    f"FloorpReleaseImportSmoke-{library['variant']}-"
                    f"{target.split('-', 1)[0]}.o"
                )
                run(
                    "/usr/bin/xcrun",
                    "--sdk",
                    sdk,
                    "swiftc",
                    "-target",
                    target,
                    "-sdk",
                    sdk_path,
                    "-F",
                    str(framework_search_path),
                    "-parse-as-library",
                    "-c",
                    str(source_path),
                    "-o",
                    str(object_path),
                )
                checked_targets.append(target)
    return sorted(checked_targets)


def validate_xcframework(
    path: pathlib.Path,
    root_name: str,
    config: dict[str, Any],
    require_floorp_binding: bool,
) -> dict[str, Any]:
    with zipfile.ZipFile(path) as archive:
        names = validate_zip_members(archive)
        plist_name = f"{root_name}/Info.plist"
        if plist_name not in names:
            raise ValueError(f"{path.name} has no root Info.plist")
        plist = plistlib.loads(archive.read(plist_name))
        if plist.get("CFBundlePackageType") != "XFWK":
            raise ValueError(f"{path.name} is not an XCFramework")

        expected_device = set(config["ios"]["device_architectures"])
        expected_simulator = set(config["ios"]["simulator_architectures"])
        found_variants: set[str] = set()
        recorded_libraries: list[dict[str, Any]] = []
        swift_import_libraries: list[dict[str, Any]] = []
        for library in plist.get("AvailableLibraries", []):
            if library.get("SupportedPlatform") != "ios":
                raise ValueError(f"unexpected platform in {path.name}: {library}")
            variant = library.get("SupportedPlatformVariant", "device")
            if variant not in {"device", "simulator"} or variant in found_variants:
                raise ValueError(
                    f"unexpected or duplicate XCFramework variant: {variant}"
                )
            found_variants.add(variant)
            expected = expected_simulator if variant == "simulator" else expected_device
            declared = set(library.get("SupportedArchitectures", []))
            if declared != expected:
                raise ValueError(
                    f"{path.name} declares {declared} for {variant}; expected {expected}"
                )

            identifier = library["LibraryIdentifier"]
            library_path = library["LibraryPath"]
            framework_name = pathlib.PurePosixPath(library_path).stem
            framework_root = f"{root_name}/{identifier}/{library_path}"
            binary_name = f"{framework_root}/{framework_name}"
            umbrella_name = f"{framework_root}/Headers/MozillaRustComponents.h"
            modulemap_name = f"{framework_root}/Modules/module.modulemap"
            if (
                binary_name not in names
                or umbrella_name not in names
                or modulemap_name not in names
            ):
                raise ValueError(
                    f"incomplete {variant} framework slice in {path.name}"
                )

            inspection = inspect_binary(
                archive.read(binary_name), os.environ.get("RUNNER_TEMP")
            )
            actual = inspection["architectures"]
            if actual != expected:
                raise ValueError(
                    f"{path.name} binary has {actual} for {variant}; "
                    f"expected {expected}"
                )
            expected_platform = "IOSSIMULATOR" if variant == "simulator" else "IOS"
            validate_build_versions(
                inspection["build_versions"],
                expected_platform,
                config["ios"]["deployment_target"],
            )
            validate_modulemap(archive.read(modulemap_name).decode("utf-8"))

            umbrella = archive.read(umbrella_name).decode("utf-8")
            ffi_name = f"{framework_root}/Headers/floorp_prefs_syncFFI.h"
            has_floorp_import = '#import "floorp_prefs_syncFFI.h"' in umbrella
            symbol_architectures = {
                architecture
                for architecture, symbols in inspection[
                    "symbols_by_architecture"
                ].items()
                if FLOORP_CONTRACT_SYMBOL in symbols
            }
            if require_floorp_binding:
                if not has_floorp_import or ffi_name not in names:
                    raise ValueError(
                        f"Floorp preferences FFI is missing from {variant}"
                    )
                ffi_header = archive.read(ffi_name).decode("utf-8")
                missing_symbol_architectures = sorted(expected - symbol_architectures)
                if (
                    FLOORP_CONTRACT_SYMBOL not in ffi_header
                    or missing_symbol_architectures
                ):
                    raise ValueError(
                        "Floorp preferences FFI symbol is missing from "
                        f"{variant} architectures: {missing_symbol_architectures}"
                    )
            elif has_floorp_import or ffi_name in names or symbol_architectures:
                raise ValueError(
                    "Focus unexpectedly contains the Floorp preferences FFI"
                )

            recorded_libraries.append(
                {
                    "variant": variant,
                    "architectures": sorted(actual),
                    "library_identifier": identifier,
                    "build_versions": sorted(
                        inspection["build_versions"],
                        key=lambda item: (item["platform"], item["minos"]),
                    ),
                }
            )
            swift_import_libraries.append(
                {
                    "variant": variant,
                    "library_identifier": identifier,
                    "architectures": sorted(actual),
                }
            )

        if found_variants != {"device", "simulator"}:
            raise ValueError(f"{path.name} does not contain both required iOS variants")

        swift_import_targets = validate_swift_imports(
            archive,
            root_name,
            swift_import_libraries,
            config["ios"]["deployment_target"],
            FLOORP_CONTRACT_SYMBOL if require_floorp_binding else None,
            os.environ.get("RUNNER_TEMP"),
        )

    swiftpm_checksum = run("swift", "package", "compute-checksum", str(path))
    digest = sha256(path)
    if swiftpm_checksum != digest:
        raise ValueError(f"SwiftPM and SHA-256 checksums differ for {path.name}")
    return {
        "sha256": digest,
        "swiftpm_checksum": swiftpm_checksum,
        "size": path.stat().st_size,
        "libraries": sorted(recorded_libraries, key=lambda item: item["variant"]),
        "swift_import_targets": sorted(swift_import_targets),
    }


def validate_swift_archive_members(
    archive: tarfile.TarFile,
) -> tuple[list[tarfile.TarInfo], set[str]]:
    members = archive.getmembers()
    unsafe = [member.name for member in members if not safe_archive_name(member.name)]
    if unsafe:
        raise ValueError(f"unsafe Swift archive members: {unsafe[:3]}")
    normalized_names = [normalized_archive_name(member.name) for member in members]
    if len(set(normalized_names)) != len(normalized_names):
        raise ValueError("Swift archive contains duplicate or colliding members")
    links = [member.name for member in members if member.issym() or member.islnk()]
    if links:
        raise ValueError(f"Swift archive must not contain links: {links[:3]}")
    special = [
        member.name for member in members if not member.isfile() and not member.isdir()
    ]
    if special:
        raise ValueError(f"Swift archive contains special files: {special[:3]}")
    names = {member.name.rstrip("/") for member in members}
    unexpected_roots = sorted(
        name
        for name in names
        if pathlib.PurePosixPath(name).parts[0] != "swift-components"
    )
    if unexpected_roots:
        raise ValueError(
            "Swift archive members must be rooted under swift-components: "
            f"{unexpected_roots[:3]}"
        )
    required = {
        "swift-components/all/Generated/floorp_prefs_sync.swift",
        "swift-components/all/Generated/floorp_prefs_syncFFI.h",
    }
    missing = sorted(required - names)
    if missing:
        raise ValueError(f"missing generated Floorp bindings: {missing}")
    if not any(
        name.startswith("swift-components/focus/Generated/")
        and name.endswith(".swift")
        for name in names
    ):
        raise ValueError("Focus generated Swift bindings are missing")
    return members, names


def extract_validated_swift_archive(
    archive: tarfile.TarFile,
    members: list[tarfile.TarInfo],
    destination: pathlib.Path,
) -> None:
    for member in members:
        output = destination.joinpath(*pathlib.PurePosixPath(member.name).parts)
        if member.isdir():
            output.mkdir(parents=True, exist_ok=True)
            continue
        output.parent.mkdir(parents=True, exist_ok=True)
        source = archive.extractfile(member)
        if source is None:
            raise ValueError(f"could not read Swift archive member: {member.name}")
        with source, output.open("wb") as stream:
            shutil.copyfileobj(source, stream)


def validate_floorp_swift_wrapper(
    swift_archive_path: pathlib.Path,
    xcframework_path: pathlib.Path,
    root_name: str,
    libraries: list[dict[str, Any]],
    deployment_target: str,
    runner_temp: str | None,
) -> list[str]:
    temp_parent = pathlib.Path(runner_temp) if runner_temp else None
    with tempfile.TemporaryDirectory(dir=temp_parent) as directory:
        extraction_root = pathlib.Path(directory)
        framework_extraction_root = extraction_root / "framework"
        swift_extraction_root = extraction_root / "swift"
        framework_extraction_root.mkdir()
        swift_extraction_root.mkdir()
        with zipfile.ZipFile(xcframework_path) as framework_archive:
            validate_zip_members(framework_archive)
            framework_archive.extractall(framework_extraction_root)
        with tarfile.open(swift_archive_path, mode="r:xz") as swift_archive:
            members, _ = validate_swift_archive_members(swift_archive)
            extract_validated_swift_archive(
                swift_archive, members, swift_extraction_root
            )

        wrapper_path = (
            swift_extraction_root
            / "swift-components/all/Generated/floorp_prefs_sync.swift"
        )
        smoke_path = extraction_root / "FloorpReleaseGeneratedBindingSmoke.swift"
        smoke_path.write_text(
            "private func floorpReleaseGeneratedBindingSmoke(\n"
            "    _ store: FloorpPrefsSyncStore.Type,\n"
            "    _ state: FloorpPrefsSyncState.Type\n"
            ") {}\n",
            encoding="utf-8",
        )
        checked_targets: list[str] = []
        for library in libraries:
            if library["variant"] == "simulator":
                sdk = "iphonesimulator"
                targets = [
                    f"{architecture}-apple-ios{deployment_target}-simulator"
                    for architecture in library["architectures"]
                ]
            else:
                sdk = "iphoneos"
                targets = [
                    f"{architecture}-apple-ios{deployment_target}"
                    for architecture in library["architectures"]
                ]
            sdk_path = run("/usr/bin/xcrun", "--sdk", sdk, "--show-sdk-path")
            framework_search_path = (
                framework_extraction_root
                / root_name
                / library["library_identifier"]
            )
            for target in targets:
                run(
                    "/usr/bin/xcrun",
                    "--sdk",
                    sdk,
                    "swiftc",
                    "-target",
                    target,
                    "-sdk",
                    sdk_path,
                    "-F",
                    str(framework_search_path),
                    "-module-name",
                    "FloorpReleaseGeneratedBindingSmoke",
                    "-parse-as-library",
                    "-typecheck",
                    str(wrapper_path),
                    str(smoke_path),
                )
                checked_targets.append(target)
    return sorted(checked_targets)


def validate_swift_archive(path: pathlib.Path) -> dict[str, Any]:
    with tarfile.open(path, mode="r:xz") as archive:
        validate_swift_archive_members(archive)
    return {"sha256": sha256(path), "size": path.stat().st_size}


def command_version(*command: str) -> str:
    return run(*command).replace("\n", "; ")


def write_metadata(
    artifacts_dir: pathlib.Path,
    config: dict[str, Any],
    release_tag: str,
    source_commit: str,
    upstream_ref: str,
    actual_merge_base: str,
    repository: str,
    workflow_url: str,
    artifact_data: dict[str, Any],
) -> None:
    source_timestamp = run("git", "show", "-s", "--format=%cI", source_commit)
    manifest = {
        "schema_version": 1,
        "release_tag": release_tag,
        "release_revision": int(release_tag.rsplit(".", 1)[1]),
        "repository": repository,
        "source": {
            "commit": source_commit,
            "tree": run("git", "rev-parse", f"{source_commit}^{{tree}}"),
            "timestamp": source_timestamp,
        },
        "upstream": {
            **config["upstream"],
            "reference": upstream_ref,
            "actual_merge_base": actual_merge_base,
        },
        "toolchain": {
            **config["toolchain"],
            "actual_xcode": command_version("xcodebuild", "-version"),
            "actual_rustc": command_version("rustc", "--version", "--verbose"),
            "actual_python": sys.version.split()[0],
            "actual_ninja": command_version("ninja", "--version"),
        },
        "ios": config["ios"],
        "nss": config["nss"],
        "workflow_url": workflow_url,
        "immutable_release_required": True,
        "artifacts": artifact_data,
    }
    manifest_path = artifacts_dir / MANIFEST
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    checksum_paths = [artifacts_dir / name for name in config["artifacts"]]
    checksum_paths.append(manifest_path)
    checksum_text = "".join(
        f"{sha256(path)}  {path.name}\n" for path in sorted(checksum_paths)
    )
    (artifacts_dir / CHECKSUMS).write_text(checksum_text, encoding="utf-8")


def validate_artifacts(
    artifacts_dir: pathlib.Path, config: dict[str, Any]
) -> dict[str, Any]:
    if not artifacts_dir.is_dir():
        raise ValueError(f"artifact directory does not exist: {artifacts_dir}")
    for name in config["artifacts"]:
        path = artifacts_dir / name
        if not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"missing or empty artifact: {name}")

    main_data = validate_xcframework(
        artifacts_dir / MAIN_ARCHIVE,
        "MozillaRustComponents.xcframework",
        config,
        require_floorp_binding=True,
    )
    focus_data = validate_xcframework(
        artifacts_dir / FOCUS_ARCHIVE,
        "FocusRustComponents.xcframework",
        config,
        require_floorp_binding=False,
    )
    swift_data = validate_swift_archive(artifacts_dir / SWIFT_ARCHIVE)
    swift_data["floorp_wrapper_targets"] = validate_floorp_swift_wrapper(
        artifacts_dir / SWIFT_ARCHIVE,
        artifacts_dir / MAIN_ARCHIVE,
        "MozillaRustComponents.xcframework",
        main_data["libraries"],
        config["ios"]["deployment_target"],
        os.environ.get("RUNNER_TEMP"),
    )
    data = {
        MAIN_ARCHIVE: main_data,
        FOCUS_ARCHIVE: focus_data,
        SWIFT_ARCHIVE: swift_data,
    }
    return data


def main() -> None:
    args = parse_args()
    config = load_config(args.config)
    actual_merge_base = validate_source(
        config,
        args.release_tag,
        args.source_commit,
        args.upstream_ref,
        args.repository,
    )
    if args.source_only:
        print("Floorp iOS release source metadata is valid.")
        return

    artifact_data = validate_artifacts(args.artifacts, config)
    if args.write_metadata:
        write_metadata(
            args.artifacts,
            config,
            args.release_tag,
            args.source_commit,
            args.upstream_ref,
            actual_merge_base,
            args.repository,
            args.workflow_url,
            artifact_data,
        )
    print(json.dumps(artifact_data, indent=2, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
