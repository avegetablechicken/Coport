"""Select a stable local macOS signing identity; print it for build scripts.

An explicit identity must exist. '-' explicitly selects ad-hoc signing.
Without an override, prefer Apple Development and fall back to ad hoc.
Only certificate identity metadata is queried, never application credentials.
"""
import re
import subprocess
import sys


def select_identity(wanted, listing):
    wanted = wanted.strip()
    if wanted == "-":
        return "-"
    identities = re.findall(r'\d+\) ([0-9A-F]{40}) "([^"]+)"', listing)
    for sha1, name in identities:
        if (sha1 == wanted.upper() or name == wanted) if wanted else name.startswith("Apple Development:"):
            return name
    if wanted:
        raise ValueError(f"Signing identity not found: {wanted}")
    return "-"


def main():
    wanted = sys.argv[1] if len(sys.argv) > 1 else ""
    if wanted.strip() == "-":
        print("-")
        return
    listing = subprocess.check_output(
        ["security", "find-identity", "-v", "-p", "codesigning"], text=True
    )
    try:
        print(select_identity(wanted, listing))
    except ValueError as error:
        raise SystemExit(str(error)) from error


if __name__ == "__main__":
    main()
