#!/usr/bin/env python3
"""Install the project guard without replacing an existing hook."""

from pathlib import Path
import os
import subprocess
import sys


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    result = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=root, check=True, capture_output=True, text=True,
    )
    hooks = Path(result.stdout.strip()) / "hooks"
    hooks.mkdir(parents=True, exist_ok=True)
    target = root / ".githooks/pre-commit"
    hook = hooks / "pre-commit"
    if os.path.lexists(hook):
        if not hook.is_symlink() or hook.resolve() != target:
            print("Existing pre-commit hook preserved. Chain .githooks/pre-commit from it.", file=sys.stderr)
            return 1
    else:
        hook.symlink_to(os.path.relpath(target, hooks))
    print("Installed the project pre-commit hook in the Git hooks directory.")
    print("A configured core.hooksPath must delegate to this directory.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
