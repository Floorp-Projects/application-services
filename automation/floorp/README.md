# Floorp iOS XCFramework distribution

Floorp publishes its custom Application Services build from the public
`Floorp-Projects/application-services` fork. A release tag identifies the Rust
source that produced all binary and generated Swift artifacts. Floorp iOS pins
the immutable release URL and its SwiftPM checksum.

## Repository settings required before the first release

1. Create `floorp-ios` directly from the exact Application Services baseline in
   `ios-xcframework-release-config.json`, make it the default branch, and protect
   it with the **Floorp iOS XCFramework required check**. Keep `main` as a clean
   Mozilla upstream mirror. Do not rebase Floorp patches onto a newer moving
   `main` merely to make a release check pass.
2. Protect lightweight tags matching `floorp-ios-*`; restrict tag creation to
   release maintainers and disallow tag updates and deletion. Protect `main`
   against direct or forced Floorp changes as well.
3. Create an environment named `floorp-xcframework-release` with a required
   reviewer. Select **Selected branches and tags** and add exactly one tag rule,
   `floorp-ios-*`; the **Protected branches only** option does not admit tag
   deployments. Enable prevent-self-review when a second release maintainer is
   available. Only the publish job receives `contents: write`. Define the
   environment variable
   `FLOORP_XCFRAMEWORK_RELEASE_GUARD=required-review-v1` in that environment only;
   a missing, automatically-created, or differently scoped environment fails
   before any release mutation.
4. In repository settings, enable **Release immutability**. The workflow verifies
   the live repository setting through the immutable-releases REST endpoint
   before it creates or modifies a draft. That endpoint requires repository
   Administration (read), which the standard `GITHUB_TOKEN` cannot request.
   Add an environment secret named `FLOORP_RELEASE_ADMIN_READ_TOKEN` containing
   a dedicated fine-grained token limited to this repository, Administration
   (read), and Actions (read). It is used only for the protected-environment and
   deployment-policy reads and the two immutable-setting reads; the short-lived
   `GITHUB_TOKEN` still owns every release read and mutation. Do not reuse a
   broad maintainer token. A GitHub App is an alternative only after the
   workflow is extended to mint a fresh installation token from its App ID and
   private key during each release; storing an installation token itself is not
   viable because it expires.
   The workflow uploads every asset to a draft and publishes only after
   verifying GitHub's asset digests. Published assets are never replaced; issue
   a new revision tag.
5. Set the repository's default workflow token permission to read-only and do
   not allow Actions to approve pull requests.

No Apple signing identity, App Store Connect key, broad personal access token,
or user-managed signing key belongs in this repository. GitHub's short-lived
`GITHUB_TOKEN` publishes the release, the restricted read credential inspects
the protected environment, deployment policy, and immutable-release setting,
and OIDC creates artifact attestations.

## Release sequence

1. Merge a reviewed source change into protected `floorp-ios` after the required
   aggregate CI check passes. Documentation-only changes still produce this
   check; native Rust and iOS jobs are skipped only for an explicit safe-docs
   allowlist.
2. Create a lightweight tag matching the pattern in
   `ios-xcframework-release-config.json`, for example
   `floorp-ios-155.20260731050244.1`.
   Tag a commit from the protected branch's first-parent history. A side-parent
   PR commit is an ancestor but was never the protected branch state, so the
   workflow rejects it.
3. The tag starts a clean macOS build. A manual run may target the same existing
   tag, but the workflow dispatch ref must be that tag itself. For example:

   ```sh
   gh workflow run floorp-ios-xcframework-release.yml \
     --ref floorp-ios-155.20260731050244.1 \
     -f release_tag=floorp-ios-155.20260731050244.1
   ```

   A dispatch from `floorp-ios`, `main`, or any SHA different from the tag source
   is rejected before attestation.
4. Approve the `floorp-xcframework-release` environment after the build and
   attestation jobs pass.
5. The publish job re-resolves the remote tag, checks the protected-environment
   sentinel, confirms repository immutability, and then verifies every remote
   asset digest before publication. It confirms that the published release is
   immutable. A rerun accepts an existing published release only when its tag,
   release state, asset set, and every digest exactly match.
6. Update Floorp iOS from `release-manifest.json`, including the binary URLs,
   checksums, and `swift-components.tar.xz` generated bindings. Run Floorp's
   normal remote-package CI before Internal TestFlight.

The release contains:

- `MozillaRustComponents.xcframework.zip`
- `FocusRustComponents.xcframework.zip`
- `swift-components.tar.xz`
- `release-manifest.json`
- `SHA256SUMS`

`release-manifest.json` records the exact Floorp source SHA, configured upstream
base SHA, actual source-to-`main` merge-base and reference, toolchain,
architectures, file sizes, and SHA-256 values. `SHA256SUMS` covers the three
distributable inputs and the manifest itself.

## Updating Application Services

Choose the Application Services commit consumed by the matching Firefox iOS
baseline, rather than an arbitrary moving `main`. Update the `upstream` object
in the release config, merge that exact upstream commit into `floorp-ios`
through a reviewed PR, and resolve the Floorp patch normally. The workflow
requires `git merge-base(<tag source>, origin/main)` to equal the configured
commit exactly, while also requiring the tag source to appear in the protected
`floorp-ios` first-parent history. Never rewrite an already released source tag
or rebase released `floorp-ios` history. Update the mirror `main` separately
from the Floorp release branch; do not use a one-click fork sync that targets
`floorp-ios`.

The build intentionally uses temporary `CARGO_HOME` and `RUSTUP_HOME`
directories under `RUNNER_TEMP`. It rejects a toolchain home outside that
directory. NSS, Rust, rustup-init, Ninja, GYP, Xcode, Python, and Glean Parser
versions are pinned. The release build does not promote a pull-request build or
an Actions cache into a published binary.

The archive timestamps and Glean build date are normalized. Rust, Apple SDK,
and native linker output are still treated as provenance-reproducible rather
than guaranteed bit-for-bit reproducible until a two-run comparison is added.
