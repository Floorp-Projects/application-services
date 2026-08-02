# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import importlib.util
import io
import json
import pathlib
import plistlib
import stat
import subprocess
import tarfile
import tempfile
import unittest
import unittest.mock
import warnings
import zipfile


SCRIPT = pathlib.Path(__file__).parents[1] / "verify-ios-xcframework-release.py"
SPEC = importlib.util.spec_from_file_location("floorp_release_verifier", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"could not load {SCRIPT}")
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)


class ReleaseVerifierTests(unittest.TestCase):
    def setUp(self):
        self.config = VERIFIER.load_config(
            pathlib.Path(__file__).parents[1] / "ios-xcframework-release-config.json"
        )

    def write_xcframework(
        self,
        path,
        root_name,
        include_floorp,
        include_modulemap=True,
        modulemap_omitted_headers=(),
        modulemap_extra_headers=(),
        include_modulemap_export=True,
    ):
        framework_name = "MozillaRustComponents"
        libraries = [
            {
                "LibraryIdentifier": "ios-arm64",
                "LibraryPath": f"{framework_name}.framework",
                "SupportedArchitectures": ["arm64"],
                "SupportedPlatform": "ios",
            },
            {
                "LibraryIdentifier": "ios-arm64_x86_64-simulator",
                "LibraryPath": f"{framework_name}.framework",
                "SupportedArchitectures": ["arm64", "x86_64"],
                "SupportedPlatform": "ios",
                "SupportedPlatformVariant": "simulator",
            },
        ]
        plist = {
            "CFBundlePackageType": "XFWK",
            "AvailableLibraries": libraries,
        }
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr(f"{root_name}/Info.plist", plistlib.dumps(plist))
            for library in libraries:
                framework_root = (
                    f"{root_name}/{library['LibraryIdentifier']}/"
                    f"{library['LibraryPath']}"
                )
                binary = (
                    b"device"
                    if "SupportedPlatformVariant" not in library
                    else b"simulator"
                )
                umbrella = (
                    '#import "floorp_prefs_syncFFI.h"\n' if include_floorp else ""
                )
                archive.writestr(f"{framework_root}/{framework_name}", binary)
                archive.writestr(
                    f"{framework_root}/Headers/MozillaRustComponents.h", umbrella
                )
                archive.writestr(
                    f"{framework_root}/Headers/RustViaductFFI.h", b"generated"
                )
                archive.writestr(
                    f"{framework_root}/Headers/nimbusFFI.h", b"generated"
                )
                if include_modulemap:
                    modulemap_headers = ["RustViaductFFI.h"]
                    if include_floorp:
                        modulemap_headers.append("floorp_prefs_syncFFI.h")
                    modulemap_headers.append("nimbusFFI.h")
                    modulemap_headers = [
                        header
                        for header in modulemap_headers
                        if header not in modulemap_omitted_headers
                    ]
                    modulemap_headers.extend(modulemap_extra_headers)
                    modulemap = "framework module MozillaRustComponents {\n"
                    modulemap += "".join(
                        f'  header "{header}"\n' for header in modulemap_headers
                    )
                    if include_modulemap_export:
                        modulemap += "  export *\n"
                    modulemap += (
                        '  use "Darwin"\n'
                        '  use "_Builtin_stdbool"\n'
                        '  use "_Builtin_stdint"\n'
                        "}\n"
                    )
                    archive.writestr(
                        f"{framework_root}/Modules/module.modulemap",
                        modulemap,
                    )
                if include_floorp:
                    archive.writestr(
                        f"{framework_root}/Headers/floorp_prefs_syncFFI.h",
                        f"uint32_t {VERIFIER.FLOORP_CONTRACT_SYMBOL}(void);\n",
                    )

    @staticmethod
    def inspection(binary, minos="15.0", include_floorp=True):
        simulator = binary == b"simulator"
        architectures = {"arm64", "x86_64"} if simulator else {"arm64"}
        return {
            "architectures": architectures,
            "build_versions": [
                {
                    "platform": "IOSSIMULATOR" if simulator else "IOS",
                    "minos": minos,
                }
            ],
            "symbols_by_architecture": {
                architecture: (
                    {VERIFIER.FLOORP_CONTRACT_SYMBOL} if include_floorp else set()
                )
                for architecture in architectures
            },
        }

    @staticmethod
    def write_swift_archive(
        path,
        wrapper=b"generated",
        syncmanager_wrapper=b"generated",
        include_syncmanager=True,
        include_sync15=True,
    ):
        members = [
            (
                "swift-components/all/Generated/floorp_prefs_sync.swift",
                wrapper,
            ),
            (
                "swift-components/all/Generated/floorp_prefs_syncFFI.h",
                b"generated",
            ),
            ("swift-components/focus/Generated/nimbus.swift", b"generated"),
        ]
        if include_syncmanager:
            members.append(
                (
                    "swift-components/all/Generated/syncmanager.swift",
                    syncmanager_wrapper,
                )
            )
        if include_sync15:
            members.append(
                ("swift-components/all/Generated/sync15.swift", b"generated")
            )
        with tarfile.open(path, "w:xz") as archive:
            for name, payload in members:
                member = tarfile.TarInfo(name)
                member.size = len(payload)
                archive.addfile(member, io.BytesIO(payload))

    def test_xcframework_architectures_and_floorp_header(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(path, root_name, include_floorp=True)

            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(binary),
            ), unittest.mock.patch.object(
                VERIFIER,
                "validate_swift_imports",
                return_value=[
                    "arm64-apple-ios15.0",
                    "arm64-apple-ios15.0-simulator",
                    "x86_64-apple-ios15.0-simulator",
                ],
            ), unittest.mock.patch.object(
                VERIFIER, "run", return_value=VERIFIER.sha256(path)
            ):
                result = VERIFIER.validate_xcframework(
                    path, root_name, self.config, require_floorp_binding=True
                )

            self.assertEqual(result["sha256"], VERIFIER.sha256(path))
            self.assertEqual(
                result["libraries"],
                [
                    {
                        "variant": "device",
                        "architectures": ["arm64"],
                        "library_identifier": "ios-arm64",
                        "build_versions": [{"platform": "IOS", "minos": "15.0"}],
                    },
                    {
                        "variant": "simulator",
                        "architectures": ["arm64", "x86_64"],
                        "library_identifier": "ios-arm64_x86_64-simulator",
                        "build_versions": [
                            {"platform": "IOSSIMULATOR", "minos": "15.0"}
                        ],
                    },
                ],
            )
            self.assertEqual(
                result["swift_import_targets"],
                [
                    "arm64-apple-ios15.0",
                    "arm64-apple-ios15.0-simulator",
                    "x86_64-apple-ios15.0-simulator",
                ],
            )

    def test_xcframework_requires_modulemap(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(
                path, root_name, include_floorp=True, include_modulemap=False
            )
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(binary),
            ):
                with self.assertRaisesRegex(ValueError, "incomplete .* framework"):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_xcframework_requires_floorp_modulemap_header(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(
                path,
                root_name,
                include_floorp=True,
                modulemap_omitted_headers=("floorp_prefs_syncFFI.h",),
            )
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(binary),
            ):
                with self.assertRaisesRegex(
                    ValueError, "missing required headers.*floorp_prefs_syncFFI"
                ):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_xcframework_rejects_missing_modulemap_header_target(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(
                path,
                root_name,
                include_floorp=True,
                modulemap_extra_headers=("missingFFI.h",),
            )
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(binary),
            ):
                with self.assertRaisesRegex(
                    ValueError, "references missing headers.*missingFFI"
                ):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_xcframework_requires_rust_viaduct_modulemap_header(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(
                path,
                root_name,
                include_floorp=True,
                modulemap_omitted_headers=("RustViaductFFI.h",),
            )
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(binary),
            ):
                with self.assertRaisesRegex(
                    ValueError, "missing required headers.*RustViaductFFI"
                ):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_xcframework_requires_modulemap_export(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(
                path,
                root_name,
                include_floorp=True,
                include_modulemap_export=False,
            )
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(binary),
            ):
                with self.assertRaisesRegex(ValueError, "does not export"):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_focus_xcframework_rejects_floorp_modulemap_header(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.FOCUS_ARCHIVE
            root_name = "FocusRustComponents.xcframework"
            self.write_xcframework(
                path,
                root_name,
                include_floorp=False,
                modulemap_extra_headers=("floorp_prefs_syncFFI.h",),
            )
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(
                    binary, include_floorp=False
                ),
            ):
                with self.assertRaisesRegex(
                    ValueError, "Focus modulemap unexpectedly contains"
                ):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=False
                    )

    def test_focus_xcframework_accepts_generated_modulemap(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.FOCUS_ARCHIVE
            root_name = "FocusRustComponents.xcframework"
            self.write_xcframework(path, root_name, include_floorp=False)
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(
                    binary, include_floorp=False
                ),
            ), unittest.mock.patch.object(
                VERIFIER,
                "validate_swift_imports",
                return_value=[
                    "arm64-apple-ios15.0",
                    "arm64-apple-ios15.0-simulator",
                    "x86_64-apple-ios15.0-simulator",
                ],
            ), unittest.mock.patch.object(
                VERIFIER, "run", return_value=VERIFIER.sha256(path)
            ):
                result = VERIFIER.validate_xcframework(
                    path, root_name, self.config, require_floorp_binding=False
                )

            self.assertEqual(result["sha256"], VERIFIER.sha256(path))

    def test_xcframework_rejects_newer_deployment_target(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(path, root_name, include_floorp=True)
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(
                    binary, minos="16.0"
                ),
            ):
                with self.assertRaisesRegex(ValueError, "requires 16.0"):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_xcframework_requires_floorp_binary_symbol(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(path, root_name, include_floorp=True)
            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=lambda binary, _runner_temp: self.inspection(
                    binary, include_floorp=False
                ),
            ):
                with self.assertRaisesRegex(ValueError, "FFI symbol is missing"):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_xcframework_requires_floorp_symbol_in_every_architecture(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.MAIN_ARCHIVE
            root_name = "MozillaRustComponents.xcframework"
            self.write_xcframework(path, root_name, include_floorp=True)

            def inspection_missing_x86(binary, _runner_temp):
                inspection = self.inspection(binary)
                if binary == b"simulator":
                    inspection["symbols_by_architecture"]["x86_64"] = set()
                return inspection

            with unittest.mock.patch.object(
                VERIFIER,
                "inspect_binary",
                side_effect=inspection_missing_x86,
            ):
                with self.assertRaisesRegex(ValueError, "x86_64"):
                    VERIFIER.validate_xcframework(
                        path, root_name, self.config, require_floorp_binding=True
                    )

    def test_swift_archive_requires_floorp_and_focus_bindings(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.SWIFT_ARCHIVE
            self.write_swift_archive(path)

            result = VERIFIER.validate_swift_archive(path)
            self.assertEqual(result["sha256"], VERIFIER.sha256(path))

    def test_floorp_uniffi_config_uses_combined_swift_module(self):
        self.assertEqual(
            VERIFIER.validate_floorp_uniffi_config(),
            VERIFIER.FLOORP_SWIFT_BINDING_CONFIG,
        )

        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for name, contents in (
                (
                    "wrong-module.toml",
                    "[bindings.swift]\n"
                    'ffi_module_name = "floorp_prefs_syncFFI"\n'
                    'ffi_module_filename = "floorp_prefs_syncFFI"\n',
                ),
                (
                    "wrong-filename.toml",
                    "[bindings.swift]\n"
                    'ffi_module_name = "MozillaRustComponents"\n'
                    'ffi_module_filename = "wrongFFI"\n',
                ),
            ):
                with self.subTest(name=name):
                    path = root / name
                    path.write_text(contents, encoding="utf-8")
                    with self.assertRaisesRegex(ValueError, "Swift UniFFI config"):
                        VERIFIER.validate_floorp_uniffi_config(path)

            with self.assertRaisesRegex(ValueError, "could not load"):
                VERIFIER.validate_floorp_uniffi_config(root / "missing.toml")

            malformed = root / "wrong-section-type.toml"
            malformed.write_text('bindings = "not a table"\n', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "could not load"):
                VERIFIER.validate_floorp_uniffi_config(malformed)

    def test_swift_archive_requires_sync_manager_dependency_bindings(self):
        with tempfile.TemporaryDirectory() as directory:
            for missing_name, options in (
                ("syncmanager.swift", {"include_syncmanager": False}),
                ("sync15.swift", {"include_sync15": False}),
            ):
                with self.subTest(missing_name=missing_name):
                    path = pathlib.Path(directory) / f"missing-{missing_name}.tar.xz"
                    self.write_swift_archive(path, **options)
                    with self.assertRaisesRegex(ValueError, missing_name):
                        VERIFIER.validate_swift_archive(path)

    def test_generated_floorp_wrapper_typechecks_for_every_architecture(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            swift_archive = root / VERIFIER.SWIFT_ARCHIVE
            framework_archive = root / VERIFIER.MAIN_ARCHIVE
            framework_root = "MozillaRustComponents.xcframework"
            self.write_swift_archive(
                swift_archive,
                wrapper=b"public final class FloorpPrefsSyncStore {}\n"
                b"public struct FloorpPrefsSyncState {}\n",
            )
            self.write_xcframework(
                framework_archive, framework_root, include_floorp=True
            )
            calls = []
            compiled_sources = []

            def fake_run(*command, cwd=VERIFIER.REPO_ROOT):
                calls.append(command)
                if command[-1] == "--show-sdk-path":
                    return "/Mock.sdk"
                if "swiftc" in command:
                    sources = [
                        pathlib.Path(argument)
                        for argument in command
                        if argument.endswith(".swift")
                    ]
                    compiled_sources.append(
                        (
                            [source.name for source in sources[:-1]],
                            sources[-1].read_text(),
                        )
                    )
                return ""

            with unittest.mock.patch.object(
                VERIFIER, "run", side_effect=fake_run
            ):
                targets = VERIFIER.validate_floorp_swift_wrapper(
                    swift_archive,
                    framework_archive,
                    framework_root,
                    [
                        {
                            "variant": "device",
                            "library_identifier": "ios-arm64",
                            "architectures": ["arm64"],
                        },
                        {
                            "variant": "simulator",
                            "library_identifier": "ios-arm64_x86_64-simulator",
                            "architectures": ["arm64", "x86_64"],
                        },
                    ],
                    "15.0",
                    None,
                )

            self.assertEqual(
                targets,
                [
                    "arm64-apple-ios15.0",
                    "arm64-apple-ios15.0-simulator",
                    "x86_64-apple-ios15.0-simulator",
                ],
            )
            swiftc_calls = [call for call in calls if "swiftc" in call]
            self.assertEqual(len(swiftc_calls), 3)
            for wrapper_names, smoke in compiled_sources:
                self.assertEqual(
                    wrapper_names,
                    [
                        "sync15.swift",
                        "syncmanager.swift",
                        "floorp_prefs_sync.swift",
                    ],
                )
                self.assertIn(": FloorpPrefsSyncDelegate", smoke)
                self.assertIn("FloorpPrefsSyncPrepareInput(", smoke)
                self.assertIn("FloorpPrefsSyncFinish(", smoke)
                self.assertIn("FloorpPrefsSyncState(", smoke)
                self.assertIn(".recordMissing", smoke)
                self.assertIn(".notesKeyMissing", smoke)
                self.assertIn(".notesNull", smoke)
                self.assertIn(".notesString(value:", smoke)
                self.assertIn(".noUpload(transactionToken:", smoke)
                self.assertIn(".upload(transactionToken:", smoke)
                self.assertIn("FloorpPrefsSyncStore(delegate:", smoke)
                self.assertIn("store.syncState()", smoke)
                self.assertIn("store.registerWithSyncManager()", smoke)
                self.assertIn("let manager = SyncManager()", smoke)
                self.assertIn("manager.disconnect()", smoke)
                self.assertIn("try manager.disconnectChecked()", smoke)

    def test_corrupt_generated_floorp_wrapper_fails_typecheck(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            swift_archive = root / VERIFIER.SWIFT_ARCHIVE
            framework_archive = root / VERIFIER.MAIN_ARCHIVE
            framework_root = "MozillaRustComponents.xcframework"
            self.write_swift_archive(swift_archive, wrapper=b"not valid Swift !!!")
            self.write_xcframework(
                framework_archive, framework_root, include_floorp=True
            )

            def fake_run(*command, cwd=VERIFIER.REPO_ROOT):
                if command[-1] == "--show-sdk-path":
                    return "/Mock.sdk"
                if "swiftc" in command:
                    wrapper = next(
                        pathlib.Path(argument)
                        for argument in command
                        if argument.endswith("floorp_prefs_sync.swift")
                    )
                    if wrapper.read_text() == "not valid Swift !!!":
                        raise subprocess.CalledProcessError(1, command)
                return ""

            with unittest.mock.patch.object(
                VERIFIER, "run", side_effect=fake_run
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    VERIFIER.validate_floorp_swift_wrapper(
                        swift_archive,
                        framework_archive,
                        framework_root,
                        [
                            {
                                "variant": "device",
                                "library_identifier": "ios-arm64",
                                "architectures": ["arm64"],
                            }
                        ],
                        "15.0",
                        None,
                    )

    def test_duplicate_zip_member_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "duplicate.zip"
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr("duplicate", b"first")
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore", UserWarning)
                    archive.writestr("duplicate", b"second")
            with zipfile.ZipFile(path) as archive:
                with self.assertRaisesRegex(ValueError, "duplicate member"):
                    VERIFIER.validate_zip_members(archive)

    def test_zip_link_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "link.zip"
            link = zipfile.ZipInfo("safe-name")
            link.create_system = 3
            link.external_attr = (stat.S_IFLNK | 0o777) << 16
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr(link, "../../outside")
            with zipfile.ZipFile(path) as archive:
                with self.assertRaisesRegex(ValueError, "links or special files"):
                    VERIFIER.validate_zip_members(archive)

    def test_zip_noncanonical_member_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "noncanonical.zip"
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr("safe//name", b"payload")
            with zipfile.ZipFile(path) as archive:
                with self.assertRaisesRegex(ValueError, "unsafe ZIP members"):
                    VERIFIER.validate_zip_members(archive)

    def test_duplicate_tar_member_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.SWIFT_ARCHIVE
            with tarfile.open(path, "w:xz") as archive:
                for payload in (b"first", b"second"):
                    member = tarfile.TarInfo(
                        "swift-components/all/Generated/floorp_prefs_sync.swift"
                    )
                    member.size = len(payload)
                    archive.addfile(member, io.BytesIO(payload))
            with self.assertRaisesRegex(ValueError, "duplicate or colliding"):
                VERIFIER.validate_swift_archive(path)

    def test_tar_special_file_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.SWIFT_ARCHIVE
            with tarfile.open(path, "w:xz") as archive:
                member = tarfile.TarInfo("swift-components/fifo")
                member.type = tarfile.FIFOTYPE
                archive.addfile(member)
            with self.assertRaisesRegex(ValueError, "special files"):
                VERIFIER.validate_swift_archive(path)

    def test_tar_noncanonical_member_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.SWIFT_ARCHIVE
            with tarfile.open(path, "w:xz") as archive:
                payload = b"generated"
                member = tarfile.TarInfo("swift-components/./generated.swift")
                member.size = len(payload)
                archive.addfile(member, io.BytesIO(payload))
            with self.assertRaisesRegex(ValueError, "unsafe Swift archive members"):
                VERIFIER.validate_swift_archive(path)

    def test_swift_archive_rejects_members_outside_its_root(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / VERIFIER.SWIFT_ARCHIVE
            with tarfile.open(path, "w:xz") as archive:
                for name in (
                    "swift-components/all/Generated/floorp_prefs_sync.swift",
                    "swift-components/all/Generated/floorp_prefs_syncFFI.h",
                    "swift-components/focus/Generated/nimbus.swift",
                    "MozillaRustComponents.xcframework/Info.plist",
                ):
                    payload = b"generated"
                    member = tarfile.TarInfo(name)
                    member.size = len(payload)
                    archive.addfile(member, io.BytesIO(payload))

            with self.assertRaisesRegex(ValueError, "rooted under swift-components"):
                VERIFIER.validate_swift_archive(path)

    def test_parse_and_validate_build_versions(self):
        records = VERIFIER.parse_macho_build_versions(
            """
Load command 1
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform IOSSIMULATOR
    minos 14.0
      sdk 26.3
"""
        )
        self.assertEqual(records, [{"platform": "IOSSIMULATOR", "minos": "14.0"}])
        VERIFIER.validate_build_versions(records, "IOSSIMULATOR", "15.0")
        with self.assertRaisesRegex(ValueError, "expected IOS"):
            VERIFIER.validate_build_versions(records, "IOS", "15.0")

    def test_resolve_rust_llvm_nm_uses_pinned_sysroot_and_host(self):
        with tempfile.TemporaryDirectory() as directory:
            sysroot = pathlib.Path(directory) / "toolchain"
            llvm_nm = (
                sysroot
                / "lib/rustlib/aarch64-apple-darwin/bin/llvm-nm"
            )
            llvm_nm.parent.mkdir(parents=True)
            llvm_nm.write_text("tool", encoding="utf-8")
            llvm_nm.chmod(0o755)

            def fake_run(*command, cwd=VERIFIER.REPO_ROOT):
                if command == ("rustc", "--print", "sysroot"):
                    return str(sysroot)
                if command == ("rustc", "--version", "--verbose"):
                    return "rustc 1.91.0\nhost: aarch64-apple-darwin\n"
                self.fail(f"unexpected command: {command}")

            with unittest.mock.patch.object(
                VERIFIER, "run", side_effect=fake_run
            ):
                self.assertEqual(VERIFIER.resolve_rust_llvm_nm(), llvm_nm.resolve())

    def test_resolve_rust_llvm_nm_fails_closed_when_component_is_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            sysroot = pathlib.Path(directory) / "toolchain"

            def fake_run(*command, cwd=VERIFIER.REPO_ROOT):
                if command == ("rustc", "--print", "sysroot"):
                    return str(sysroot)
                if command == ("rustc", "--version", "--verbose"):
                    return "rustc 1.91.0\nhost: aarch64-apple-darwin\n"
                self.fail(f"unexpected command: {command}")

            with unittest.mock.patch.object(
                VERIFIER, "run", side_effect=fake_run
            ):
                with self.assertRaisesRegex(ValueError, "llvm-tools-preview"):
                    VERIFIER.resolve_rust_llvm_nm()

    def test_inspect_binary_uses_rust_llvm_nm_for_each_architecture(self):
        llvm_nm = pathlib.Path(
            "/pinned-rust/lib/rustlib/aarch64-apple-darwin/bin/llvm-nm"
        )
        calls = []

        def fake_run(*command, cwd=VERIFIER.REPO_ROOT):
            calls.append(command)
            if command[0] == "/usr/bin/lipo":
                return "x86_64 arm64"
            if command[0] == "/usr/bin/otool":
                return """
Load command 1
      cmd LC_BUILD_VERSION
 platform IOSSIMULATOR
    minos 15.0
      sdk 26.3
"""
            if command[0] == str(llvm_nm):
                architecture = command[1].split("=", 1)[1]
                return (
                    "archive(member.o):\n"
                    f"_{VERIFIER.FLOORP_CONTRACT_SYMBOL}\n"
                    f"_{architecture}_only\n"
                )
            self.fail(f"unexpected command: {command}")

        with unittest.mock.patch.object(
            VERIFIER, "resolve_rust_llvm_nm", return_value=llvm_nm
        ), unittest.mock.patch.object(VERIFIER, "run", side_effect=fake_run):
            result = VERIFIER.inspect_binary(b"universal", None)

        self.assertEqual(result["architectures"], {"arm64", "x86_64"})
        self.assertEqual(
            result["symbols_by_architecture"],
            {
                "arm64": {VERIFIER.FLOORP_CONTRACT_SYMBOL, "arm64_only"},
                "x86_64": {VERIFIER.FLOORP_CONTRACT_SYMBOL, "x86_64_only"},
            },
        )
        llvm_nm_calls = [call for call in calls if call[0] == str(llvm_nm)]
        self.assertEqual(
            [call[1] for call in llvm_nm_calls],
            ["--arch=arm64", "--arch=x86_64"],
        )
        self.assertTrue(
            all(
                call[2:6]
                == (
                    "--extern-only",
                    "--defined-only",
                    "--format=just-symbols",
                    "--quiet",
                )
                for call in llvm_nm_calls
            )
        )
        self.assertFalse(any(call[0] == "/usr/bin/nm" for call in calls))

    def test_swift_import_smoke_covers_device_and_simulator(self):
        with tempfile.TemporaryDirectory() as directory:
            archive_path = pathlib.Path(directory) / "framework.zip"
            root_name = "MozillaRustComponents.xcframework"
            with zipfile.ZipFile(archive_path, "w") as writer:
                writer.writestr(
                    f"{root_name}/ios-arm64/MozillaRustComponents.framework/"
                    "Modules/module.modulemap",
                    "framework module MozillaRustComponents {}\n",
                )
                writer.writestr(
                    f"{root_name}/ios-arm64_x86_64-simulator/"
                    "MozillaRustComponents.framework/Modules/module.modulemap",
                    "framework module MozillaRustComponents {}\n",
                )

            calls = []

            def fake_run(*command, cwd=VERIFIER.REPO_ROOT):
                calls.append(command)
                if command[-1] == "--show-sdk-path":
                    return "/Mock.sdk"
                return ""

            with zipfile.ZipFile(archive_path) as archive, unittest.mock.patch.object(
                VERIFIER, "run", side_effect=fake_run
            ):
                targets = VERIFIER.validate_swift_imports(
                    archive,
                    root_name,
                    [
                        {
                            "variant": "device",
                            "library_identifier": "ios-arm64",
                            "architectures": ["arm64"],
                        },
                        {
                            "variant": "simulator",
                            "library_identifier": "ios-arm64_x86_64-simulator",
                            "architectures": ["arm64", "x86_64"],
                        },
                    ],
                    "15.0",
                    VERIFIER.FLOORP_CONTRACT_SYMBOL,
                    None,
                )

            self.assertEqual(
                targets,
                [
                    "arm64-apple-ios15.0",
                    "arm64-apple-ios15.0-simulator",
                    "x86_64-apple-ios15.0-simulator",
                ],
            )
            swiftc_calls = [call for call in calls if "swiftc" in call]
            self.assertEqual(len(swiftc_calls), 3)
            self.assertTrue(all("-c" in call for call in swiftc_calls))

    def test_manifest_records_observed_upstream_merge_base(self):
        with tempfile.TemporaryDirectory() as directory:
            artifacts = pathlib.Path(directory)
            for name in self.config["artifacts"]:
                (artifacts / name).write_bytes(name.encode("utf-8"))
            with unittest.mock.patch.object(VERIFIER, "run", return_value="observed"):
                VERIFIER.write_metadata(
                    artifacts,
                    self.config,
                    "floorp-ios-155.20260731050244.1",
                    "a" * 40,
                    "origin/main",
                    self.config["upstream"]["commit"],
                    self.config["distribution_repository"],
                    "https://example.invalid/workflow",
                    {},
                )
            manifest = json.loads(
                (artifacts / VERIFIER.MANIFEST).read_text(encoding="utf-8")
            )
            self.assertEqual(manifest["upstream"]["reference"], "origin/main")
            self.assertEqual(
                manifest["upstream"]["actual_merge_base"],
                self.config["upstream"]["commit"],
            )

    def test_main_passes_loaded_config_to_artifact_validation(self):
        artifacts = pathlib.Path("/tmp/floorp-release-test-artifacts")
        args = unittest.mock.Mock(
            config=pathlib.Path("release-config.json"),
            release_tag="floorp-ios-155.20260731050244.1",
            source_commit="a" * 40,
            upstream_ref="origin/main",
            repository=self.config["distribution_repository"],
            source_only=False,
            artifacts=artifacts,
            write_metadata=False,
            workflow_url="https://example.invalid/workflow",
        )

        with unittest.mock.patch.object(VERIFIER, "parse_args", return_value=args), \
             unittest.mock.patch.object(
                 VERIFIER, "load_config", return_value=self.config
             ), unittest.mock.patch.object(
                 VERIFIER, "validate_source", return_value="b" * 40
             ), unittest.mock.patch.object(
                 VERIFIER, "validate_artifacts", return_value={}
             ) as validate_artifacts, unittest.mock.patch("builtins.print"):
            VERIFIER.main()

        validate_artifacts.assert_called_once_with(artifacts, self.config)

    def test_nss_configuration_rejects_late_override(self):
        build_script = (
            pathlib.Path(__file__).parents[3] / "libs/build-all.sh"
        ).read_text(encoding="utf-8")
        parsed = VERIFIER.parse_nss_build_configuration(build_script)
        self.assertEqual(parsed["NSS_ARCHIVE"], self.config["nss"]["archive"])
        self.assertEqual(parsed["NSS_SHA256"], self.config["nss"]["sha256"])

        overridden = build_script.replace(
            "# End of configuration.",
            '# End of configuration.\nNSS_SHA256="later-override"',
            1,
        )
        with self.assertRaisesRegex(ValueError, "unexpected active NSS assignment"):
            VERIFIER.parse_nss_build_configuration(overridden)


class UpstreamBaselineTests(unittest.TestCase):
    def git(self, repo, *args):
        return subprocess.check_output(
            ["git", *args], cwd=repo, text=True, stderr=subprocess.DEVNULL
        ).strip()

    def commit(self, repo, filename, contents, message):
        (repo / filename).write_text(contents, encoding="utf-8")
        subprocess.check_call(
            ["git", "add", filename],
            cwd=repo,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        subprocess.check_call(
            ["git", "-c", "commit.gpgSign=false", "commit", "-m", message],
            cwd=repo,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return self.git(repo, "rev-parse", "HEAD")

    def test_exact_fork_point_is_required(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = pathlib.Path(directory)
            subprocess.check_call(
                ["git", "init", "-q"], cwd=repo, stdout=subprocess.DEVNULL
            )
            self.git(repo, "config", "user.name", "Floorp Test")
            self.git(repo, "config", "user.email", "floorp-test@example.invalid")
            base = self.commit(repo, "base", "base\n", "base")
            self.git(repo, "branch", "upstream-main")
            self.git(repo, "checkout", "-q", "upstream-main")
            upstream_head = self.commit(repo, "upstream", "upstream\n", "upstream")
            self.git(repo, "checkout", "-q", "-b", "floorp-ios", base)
            floorp_head = self.commit(repo, "floorp", "floorp\n", "floorp")

            self.assertEqual(
                VERIFIER.validate_upstream_base(
                    base,
                    floorp_head,
                    "refs/heads/upstream-main",
                    repo,
                ),
                base,
            )

            self.git(repo, "checkout", "-q", "-b", "rebased-floorp", upstream_head)
            rebased_head = self.commit(repo, "rebased", "rebased\n", "rebased")
            with self.assertRaisesRegex(ValueError, upstream_head):
                VERIFIER.validate_upstream_base(
                    base,
                    rebased_head,
                    "refs/heads/upstream-main",
                    repo,
                )


if __name__ == "__main__":
    unittest.main()
