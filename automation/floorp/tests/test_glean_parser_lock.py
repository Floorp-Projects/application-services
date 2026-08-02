# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import hashlib
import importlib.util
import pathlib
import subprocess
import sys
import tempfile
import unittest
import zipfile


FLOORP_DIR = pathlib.Path(__file__).parents[1]
REPO_ROOT = FLOORP_DIR.parent.parent
SCRIPT = FLOORP_DIR / "verify-ios-xcframework-release.py"
SPEC = importlib.util.spec_from_file_location("floorp_release_verifier_lock", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"could not load {SCRIPT}")
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)


def write_wheel(
    directory: pathlib.Path, name: str, version: str, requires=()
) -> pathlib.Path:
    normalized = name.replace("-", "_")
    dist_info = f"{normalized}-{version}.dist-info"
    wheel_path = directory / f"{normalized}-{version}-py3-none-any.whl"
    metadata = [
        "Metadata-Version: 2.1",
        f"Name: {name}",
        f"Version: {version}",
    ]
    metadata.extend(f"Requires-Dist: {requirement}" for requirement in requires)
    with zipfile.ZipFile(wheel_path, "w") as wheel:
        wheel.writestr(f"{normalized}/__init__.py", "VALUE = 'installed'\n")
        wheel.writestr(f"{dist_info}/METADATA", "\n".join(metadata) + "\n")
        wheel.writestr(
            f"{dist_info}/WHEEL",
            "Wheel-Version: 1.0\n"
            "Generator: floorp-release-test\n"
            "Root-Is-Purelib: true\n"
            "Tag: py3-none-any\n",
        )
        wheel.writestr(f"{dist_info}/RECORD", "")
    return wheel_path


def digest(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class GleanParserLockTests(unittest.TestCase):
    def pip_install(self, requirements, wheelhouse, target):
        return subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--no-cache-dir",
                "--no-index",
                "--find-links",
                str(wheelhouse),
                "--require-hashes",
                "--only-binary=:all:",
                "--target",
                str(target),
                "--requirement",
                str(requirements),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )

    def test_release_lock_is_complete_and_installer_is_fail_closed(self):
        requirements = VERIFIER.parse_hashed_requirements(
            FLOORP_DIR / "glean-parser-requirements.txt"
        )
        self.assertEqual(len(requirements), 13)
        self.assertEqual(requirements["glean-parser"]["version"], "19.2.0")
        self.assertTrue(all(item["hashes"] for item in requirements.values()))

        generator = (REPO_ROOT / "tools/sdk_generator.sh").read_text(encoding="utf-8")
        self.assertIn("--require-hashes", generator)
        self.assertIn("--only-binary=:all:", generator)

    def test_clean_hash_locked_install_succeeds(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            wheel = write_wheel(root, "locked-root", "1.0")
            requirements = root / "requirements.txt"
            requirements.write_text(
                f"locked-root==1.0 --hash=sha256:{digest(wheel)}\n",
                encoding="utf-8",
            )
            result = self.pip_install(requirements, root, root / "installed")
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertTrue((root / "installed/locked_root/__init__.py").is_file())

    def test_tampered_wheel_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            wheel = write_wheel(root, "locked-root", "1.0")
            expected = digest(wheel)
            with wheel.open("ab") as stream:
                stream.write(b"tampered")
            requirements = root / "requirements.txt"
            requirements.write_text(
                f"locked-root==1.0 --hash=sha256:{expected}\n", encoding="utf-8"
            )
            result = self.pip_install(requirements, root, root / "installed")
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("THESE PACKAGES DO NOT MATCH THE HASHES", result.stdout)

    def test_unlisted_transitive_dependency_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            write_wheel(root, "missing-dependency", "1.0")
            wheel = write_wheel(
                root,
                "locked-root",
                "1.0",
                requires=("missing-dependency==1.0",),
            )
            requirements = root / "requirements.txt"
            requirements.write_text(
                f"locked-root==1.0 --hash=sha256:{digest(wheel)}\n",
                encoding="utf-8",
            )
            result = self.pip_install(requirements, root, root / "installed")
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("--require-hashes mode", result.stdout)


if __name__ == "__main__":
    unittest.main()
