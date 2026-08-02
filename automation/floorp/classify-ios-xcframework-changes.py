#!/usr/bin/env python3

"""Classify whether a Floorp iOS CI change needs native build jobs."""

from __future__ import annotations

import pathlib
import sys
from collections.abc import Iterable


SAFE_EXACT_PATHS = {
    "README.md",
    "CODE_OF_CONDUCT.md",
    "CONTRIBUTING.md",
    "LICENSE",
    ".github/PULL_REQUEST_TEMPLATE.md",
}
SAFE_PREFIXES = ("docs/", ".github/ISSUE_TEMPLATE/")


def is_safe_documentation_path(path: str) -> bool:
    normalized = pathlib.PurePosixPath(path).as_posix()
    return (
        normalized in SAFE_EXACT_PATHS
        or normalized.startswith("LICENSE.")
        or normalized.startswith(SAFE_PREFIXES)
    )


def requires_native_build(paths: Iterable[str]) -> bool:
    """Fail safe: every path outside the explicit docs allowlist needs a build."""

    for path in paths:
        normalized = path.strip()
        if normalized and not is_safe_documentation_path(normalized):
            return True
    return False


def main() -> None:
    print("true" if requires_native_build(sys.stdin) else "false")


if __name__ == "__main__":
    main()
