# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at http://mozilla.org/MPL/2.0/.

import importlib.util
import json
import pathlib
import subprocess
import tempfile
import unittest


REPO_ROOT = pathlib.Path(__file__).parents[3]
AUTOMATION = pathlib.Path(__file__).parents[1]
CLASSIFIER_PATH = AUTOMATION / "classify-ios-xcframework-changes.py"
CLASSIFIER_SPEC = importlib.util.spec_from_file_location(
    "floorp_ios_change_classifier", CLASSIFIER_PATH
)
if CLASSIFIER_SPEC is None or CLASSIFIER_SPEC.loader is None:
    raise RuntimeError(f"could not load {CLASSIFIER_PATH}")
CLASSIFIER = importlib.util.module_from_spec(CLASSIFIER_SPEC)
CLASSIFIER_SPEC.loader.exec_module(CLASSIFIER)


class FloorpIOSWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflows = REPO_ROOT / ".github" / "workflows"
        cls.ci = (workflows / "floorp-ios-xcframework-ci.yml").read_text(
            encoding="utf-8"
        )
        cls.release = (
            workflows / "floorp-ios-xcframework-release.yml"
        ).read_text(encoding="utf-8")
        cls.config = json.loads(
            (AUTOMATION / "ios-xcframework-release-config.json").read_text(
                encoding="utf-8"
            )
        )
        cls.rust_installer = (
            AUTOMATION / "install-rust-toolchain.sh"
        ).read_text(encoding="utf-8")

    def test_runner_compatible_toolchain_bootstrap(self):
        self.assertEqual(self.config["toolchain"]["python"], "3.12.10")
        self.assertIn(
            'installer="${installer_dir}/rustup-init"', self.rust_installer
        )
        self.assertNotIn(
            'installer="${RUNNER_TEMP}/floorp-rustup-init"', self.rust_installer
        )

    def test_docs_allowlist_is_explicit_and_unknown_paths_build(self):
        self.assertFalse(
            CLASSIFIER.requires_native_build(
                ["README.md\n", "docs/release-process.md\n", "\n"]
            )
        )
        for build_input in (
            ".cargo/config.toml",
            ".github/workflows/floorp-ios-xcframework-ci.yml",
            "automation/floorp/README.md",
            "components/floorp-prefs-sync/src/lib.rs",
            "libs/build-all.sh",
            "megazords/ios-rust/generate-files.sh",
            "tools/uniffi-bindgen-library-mode/src/main.rs",
            "previously-unknown-build-input.txt",
        ):
            with self.subTest(build_input=build_input):
                self.assertTrue(CLASSIFIER.requires_native_build([build_input]))

    def test_source_renamed_into_docs_still_requires_native_build(self):
        with tempfile.TemporaryDirectory() as directory:
            repository = pathlib.Path(directory)
            subprocess.check_call(["git", "init", "-q"], cwd=repository)
            subprocess.check_call(
                ["git", "config", "user.name", "Floorp CI Test"], cwd=repository
            )
            subprocess.check_call(
                ["git", "config", "user.email", "ci@example.invalid"],
                cwd=repository,
            )
            source = repository / "components/native.rs"
            source.parent.mkdir()
            source.write_text("native\n", encoding="utf-8")
            subprocess.check_call(["git", "add", "."], cwd=repository)
            subprocess.check_call(
                ["git", "-c", "commit.gpgSign=false", "commit", "-qm", "source"],
                cwd=repository,
            )

            destination = repository / "docs/native.rs"
            destination.parent.mkdir()
            source.rename(destination)
            subprocess.check_call(["git", "add", "-A"], cwd=repository)
            subprocess.check_call(
                ["git", "-c", "commit.gpgSign=false", "commit", "-qm", "move"],
                cwd=repository,
            )
            changed_paths = subprocess.check_output(
                [
                    "git",
                    "diff",
                    "--no-renames",
                    "--name-only",
                    "HEAD^",
                    "HEAD",
                ],
                cwd=repository,
                text=True,
            ).splitlines()

            self.assertEqual(changed_paths, ["components/native.rs", "docs/native.rs"])
            self.assertTrue(CLASSIFIER.requires_native_build(changed_paths))

    def test_ci_always_creates_one_required_check_for_floorp_ios(self):
        trigger = self.ci.split("\npermissions:", 1)[0]
        self.assertEqual(trigger.count("branches: [floorp-ios]"), 2)
        self.assertNotIn("branches: [main]", trigger)
        self.assertNotIn("paths:", trigger)
        self.assertIn("git diff --no-renames --name-only", self.ci)
        self.assertIn("required-check:", self.ci)
        self.assertIn("name: Floorp iOS XCFramework required check", self.ci)
        self.assertIn("if: always()", self.ci)
        self.assertIn("RUN_EXPENSIVE", self.ci)

    def test_ci_fetches_and_passes_exact_upstream_reference(self):
        self.assertGreaterEqual(
            self.ci.count("+refs/heads/main:refs/remotes/origin/main"), 2
        )
        self.assertIn("--upstream-ref origin/main", self.ci)
        self.assertIn("FLOORP_IOS_UPSTREAM_REF: origin/main", self.ci)

    def test_release_requires_exact_branch_baseline_and_event_identity(self):
        for contract in (
            'expected_ref="refs/tags/$release_tag"',
            '"$EVENT_SHA" != "$source_sha"',
            "+refs/heads/floorp-ios:refs/remotes/origin/floorp-ios",
            'git merge-base --is-ancestor "$source_sha" origin/floorp-ios',
            "git rev-list --first-parent origin/floorp-ios",
            'git merge-base --all "$source_sha" origin/main',
            '"$actual_merge_base" != "$configured_upstream"',
            "--upstream-ref origin/main",
            "FLOORP_IOS_UPSTREAM_REF: origin/main",
            "UPSTREAM_MERGE_BASE:",
            'manifest["upstream"]["actual_merge_base"]',
        ):
            with self.subTest(contract=contract):
                self.assertIn(contract, self.release)
        self.assertEqual(
            self.release.count("git rev-list --first-parent origin/floorp-ios"),
            2,
        )

    def test_release_preflights_every_guard_before_mutation(self):
        guard = self.release.index("FLOORP_XCFRAMEWORK_RELEASE_GUARD")
        remote_tag = self.release.index("git/ref/tags/$RELEASE_TAG")
        immutable = self.release.index("immutable-releases")
        environment = self.release.index(
            "environments/floorp-xcframework-release"
        )
        first_mutation = min(
            self.release.index("gh release create"),
            self.release.index("gh release upload"),
            self.release.index("gh release edit"),
        )
        self.assertLess(guard, first_mutation)
        self.assertLess(environment, first_mutation)
        self.assertLess(remote_tag, first_mutation)
        self.assertLess(immutable, first_mutation)
        self.assertIn('rule.get("type") == "required_reviewers"', self.release)
        self.assertIn('"protected_branches": False', self.release)
        self.assertIn('"custom_branch_policies": True', self.release)
        self.assertIn('expected = [{"name": "floorp-ios-*", "type": "tag"}]', self.release)
        self.assertIn("deployment-branch-policies?per_page=100", self.release)
        self.assertGreaterEqual(
            self.release.count("git/ref/tags/$RELEASE_TAG"), 2
        )
        self.assertGreaterEqual(self.release.count("immutable-releases"), 2)
        self.assertEqual(
            self.release.count("secrets.FLOORP_RELEASE_ADMIN_READ_TOKEN"), 2
        )
        self.assertGreaterEqual(
            self.release.count('GH_TOKEN="$IMMUTABLE_SETTINGS_TOKEN" gh api'), 2
        )
        self.assertIn("refusing all release mutations", self.release)

    def test_matching_published_release_is_idempotent(self):
        self.assertIn('if [[ "$release_mode" == "published" ]]', self.release)
        self.assertIn("verify_release_assets published", self.release)
        self.assertIn("Existing immutable release", self.release)
        self.assertNotIn("Refusing to modify an already published release", self.release)


if __name__ == "__main__":
    unittest.main()
