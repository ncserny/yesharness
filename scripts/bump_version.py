#!/usr/bin/env python3
"""Bump YesHarness's version by 0.1 (0.2.0 -> 0.3.0, 0.9.0 -> 1.0.0) everywhere it's recorded:
Cargo.toml, Cargo.lock and the macOS Info.plist (whose build number goes up by 1).
Prints the new version. Usage: scripts/bump_version.py [repo-root]
"""
import re
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent)


def replace_once(path: Path, pattern: str, repl, flags=0) -> None:
    text = path.read_text()
    new, n = re.subn(pattern, repl, text, count=1, flags=flags)
    if n != 1:
        sys.exit(f"bump_version: couldn't find the version in {path}")
    path.write_text(new)


cargo = root / "Cargo.toml"
m = re.search(r'(?ms)^\[package\].*?^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"', cargo.read_text())
if not m:
    sys.exit("bump_version: no [package] version in Cargo.toml")
major, minor = int(m.group(1)), int(m.group(2))
minor += 1
if minor >= 10:  # "+0.1" all the way: 0.9 -> 1.0
    major, minor = major + 1, 0
new = f"{major}.{minor}.0"

replace_once(cargo, r'(?ms)(^\[package\].*?^version\s*=\s*")[^"]+(")', rf"\g<1>{new}\g<2>")
replace_once(root / "Cargo.lock", r'(name = "yesharness"\nversion = ")[^"]+(")', rf"\g<1>{new}\g<2>")
plist = root / "macos" / "Info.plist"
replace_once(plist, r"(<key>CFBundleShortVersionString</key><string>)[^<]+(</string>)", rf"\g<1>{new}\g<2>")
build = int(re.search(r"<key>CFBundleVersion</key><string>(\d+)</string>", plist.read_text()).group(1))
replace_once(plist, r"(<key>CFBundleVersion</key><string>)\d+(</string>)", rf"\g<1>{build + 1}\g<2>")
print(new)
