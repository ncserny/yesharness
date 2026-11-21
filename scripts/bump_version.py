#!/usr/bin/env python3
"""YesHarness's version lives in git tags (v1.4.0, v1.5.0, ...): every merged PR gets the next tag (+0.1).
This writes a version into Cargo.toml, Cargo.lock and the macOS Info.plist for a build, without committing:

  scripts/bump_version.py --next 1.4.0     prints 1.5.0 (0.9.0 -> 1.0.0)
  scripts/bump_version.py --set 1.5.0      writes 1.5.0 (Info.plist build number: 105)
  scripts/bump_version.py [repo-root]      old behavior: bump the files by 0.1 and print the new version
"""
import re
import sys
from pathlib import Path

def next_version(v: str) -> str:
    major, minor = (int(x) for x in v.lstrip("v").split(".")[:2])
    minor += 1
    if minor >= 10:  # "+0.1" all the way: 0.9 -> 1.0
        major, minor = major + 1, 0
    return f"{major}.{minor}.0"


args = sys.argv[1:]
if args[:1] == ["--next"]:
    print(next_version(args[1]))
    sys.exit(0)
set_to = args[1].lstrip("v") if args[:1] == ["--set"] else None
root = Path(__file__).resolve().parent.parent if set_to or not args else Path(args[0])


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
new = set_to or next_version(f"{m.group(1)}.{m.group(2)}")

replace_once(cargo, r'(?ms)(^\[package\].*?^version\s*=\s*")[^"]+(")', rf"\g<1>{new}\g<2>")
replace_once(root / "Cargo.lock", r'(name = "yesharness"\nversion = ")[^"]+(")', rf"\g<1>{new}\g<2>")
plist = root / "macos" / "Info.plist"
replace_once(plist, r"(<key>CFBundleShortVersionString</key><string>)[^<]+(</string>)", rf"\g<1>{new}\g<2>")
major, minor = (int(x) for x in new.split(".")[:2])
build = major * 100 + minor  # 1.5.0 -> 105: always goes up
replace_once(plist, r"(<key>CFBundleVersion</key><string>)\d+(</string>)", rf"\g<1>{build}\g<2>")
print(new)
