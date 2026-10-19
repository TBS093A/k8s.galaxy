#!/usr/bin/env python3
"""pre-commit hook: block private words (domains, host names, customer names, ...) from commits.

The patterns are private by nature, so they never live in the repository: put one
case-insensitive regular expression per line into ``.private-patterns`` (git-ignored)
or point ``K8S_GALAXY_PRIVATE_PATTERNS`` to a file. Without such a file the hook passes.
"""

from __future__ import annotations

import contextlib
import os
import re
import sys
from pathlib import Path


def load_patterns() -> list[re.Pattern[str]]:
    source = Path(os.environ.get("K8S_GALAXY_PRIVATE_PATTERNS", ".private-patterns"))
    if not source.is_file():
        return []
    lines = (line.strip() for line in source.read_text(encoding="utf-8").splitlines())
    return [re.compile(line, re.IGNORECASE) for line in lines if line and not line.startswith("#")]


def main(paths: list[str]) -> int:
    patterns = load_patterns()
    found = 0
    for name in paths:
        candidates = [(0, name)]  # the file name itself counts too
        with contextlib.suppress(OSError):  # deleted or unreadable files: check the name only
            candidates += enumerate(Path(name).read_text(encoding="utf-8", errors="replace").splitlines(), 1)
        for lineno, text in candidates:
            if any(p.search(text) for p in patterns):
                where = f"{name}:{lineno}" if lineno else f"{name} (file name)"
                print(f"{where}: matches a private pattern")  # never echo the match itself
                found += 1
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
