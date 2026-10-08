# RUSTSEC-2026-0258: Floorp iOS assessment

Assessment date: 2026-10-09 (JST). Source baseline:
`e1c77eb6e333ef667456504100c7b09763b8ad35` on `floorp-ios`.

## Finding and dependency paths

[Dependency Check run 37789931342](https://github.com/Floorp-Projects/application-services/actions/runs/37789931342)
reports one vulnerability, h2 0.3.26 / RUSTSEC-2026-0258, separately from 15 allowed
warnings (unmaintained, unsound and yanked dependencies). The warning classification
does not make those other findings harmless; they require separate triage.
The [RustSec advisory](https://github.com/RustSec/advisory-db/blob/main/crates/h2/RUSTSEC-2026-0258.md)
classifies this as a low-severity denial of service caused by unbounded empty DATA
frame queuing and identifies h2 >=0.4.16 as patched.

Before this change the shipping source path is:
`megazord_ios -> viaduct-hyper 0.2.0 -> hyper 0.14.27 -> h2 0.3.26`.
The Focus megazord also includes viaduct-hyper. hyper-tls 0.5 shares hyper 0.14.
The workspace also contains hyper 0.14 through axum/nimbus-cli. axum's default
features enable HTTP/1, not HTTP/2; previously viaduct-hyper enabled HTTP/2 on this
shared hyper version through Cargo feature unification.

After this change, viaduct-hyper uses hyper 1.11.1, hyper-util 0.1.21,
hyper-tls 0.6.0 and h2 0.4.20. An explicit h2 >=0.4.16 lower bound prevents
resolution below the advisory's patched floor. There is no h2 0.3 in Cargo.lock.
nimbus-cli keeps its existing HTTP/1 server stack; its tests explicitly enable the
legacy client's client/http1/tcp features instead of depending on viaduct's feature
unification. No advisory exception is added.

## iOS usage and reachability

At Floorp iOS main commit `d09fd9eb9add28aad31877d15fe540db77247849`, the following source files
call `Viaduct.shared.initialize(userAgent:)`:

- firefox-ios/Client/Application/AppDelegate.swift
- firefox-ios/Storage/Rust/RustFirefoxSuggest.swift
- firefox-ios/Extensions/NotificationService/NotificationService.swift
- firefox-ios/Extensions/ShareTo/ShareViewController.swift

[Viaduct.swift](https://github.com/Floorp-Projects/floorp-ios/blob/main/MozillaRustComponents/Sources/MozillaRustComponentsWrapper/Viaduct/Viaduct.swift)
calls `viaductInitBackendHyper()`. Therefore the Rust HTTP backend is actually
initialized by the app and extensions; it is not merely an unused workspace tool.

That does not establish exploitation in the shipped app. The existing
`HttpsConnector::new()` uses native-tls defaults and does not configure ALPN h2;
neither the production client nor this change enables HTTP/2 prior knowledge.
The source therefore does not demonstrate an ordinary HTTPS path negotiating h2.
The vulnerable HTTP/2 parser was compiled in, and feature changes or connector
configuration could expose it. Replacing it with the patched implementation avoids
relying on the current protocol configuration as a mitigation. The regression tests
deliberately use HTTP/2 prior knowledge against a loopback peer to exercise the parser.

This investigation inspected source, not network traces or the release binary.
No claim is made that an exploit has occurred or that every shipped binary is safe.

## Compatibility and upstream options

[Mozilla PR 7596](https://github.com/mozilla/application-services/pull/7596) is an
open reqwest migration that also renames the backend crate and exported initializer.
[Mozilla PR 7608](https://github.com/mozilla/application-services/pull/7608) ports
nimbus-cli's server stack. Neither change is required for this narrow fix.
[h2 PR 948](https://github.com/hyperium/h2/pull/948), a 0.3 backport, was closed
without merging. RustSec currently specifies only >=0.4.16 as patched.

This patch uses the maintained hyper 1 transport through hyper-util's legacy-client
adapter. It preserves the crate name, UniFFI initializer, Swift symbol contract,
single worker runtime, native TLS connector, manual redirect behavior, user-agent
precedence, request methods and whole-request timeout. It avoids reqwest's automatic
redirect/proxy policy and the upstream exported-function rename. It does not enable
new ALPN protocols. HTTP/body types are private implementation details.

New explicit dependencies are bytes, http-body-util, hyper-util and a security floor
on h2. bytes/h2 were already present transitively; the two adapter crates are maintained
in the hyper ecosystem and use MPL-compatible MIT licensing. Dependency summaries are
regenerated using the repository tool. Cargo.lock and --locked builds retain
reproducible dependency selection.

## Verification and release requirements

The backend's socket tests cover HTTP/1 POST method/query/headers/body/response,
settings user-agent versus explicit header, redirects and redirect limits,
timeouts while the body is stalled, legitimate empty HTTP/2 DATA and END_STREAM,
and empty/padding-only DATA floods. Floods must produce
GOAWAY(ENHANCE_YOUR_CALM, "too_many_data_frames"), not just any connection error.
The existing Floorp CI now runs these tests; Dependency Check runs on floorp-ios
pull requests and pushes as well as main.

Local verification: 7 backend tests and 5 existing nimbus-cli server tests pass;
`cargo check --locked -p nimbus-cli --tests`, backend Clippy with warnings denied,
formatting and 45 Floorp automation tests pass. `cargo audit --json` without any
ignore flags reports 0 vulnerabilities and 15 warnings (4 unmaintained, 7 unsound,
4 yanked). This is not a clean bill of health for those remaining warnings.

Before shipping, require the macOS iOS candidate build and Swift tests in the existing
XCFramework CI. Build a new immutable XCFramework release from the reviewed commit,
then update Floorp iOS's MozillaRustComponents/Package.swift URLs/checksums and generated
wrappers from that same release. It currently pins
`floorp-ios-155.20260731050244.5`; merging this source PR alone does not update installed
apps. Verify FxA login/sync, remote settings/suggestions and extension networking on
device/simulator. Linux transport tests cannot establish Apple native TLS behavior.

Remaining risks outside this fix include the audit warnings, unlimited total response
body collection, and existing redirect policy (including zero-limit semantics and
header forwarding). These are not silently changed by this transport migration.

