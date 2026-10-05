#!/usr/bin/env python3
"""Prepare and validate releases using only the Python standard library.

The workspace version (`[workspace.package].version` in Cargo.toml) is the
single release version. `check` validates the changelog against it and that
every other copy of it agrees; `sync-versions` (also run by `prepare`) writes
it to those copies.
"""

import argparse
import datetime
import json
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
# New releases use SemVer; historical four-component tags remain in the document.
SEMVER = re.compile(
    r"v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)"
    r"(?:-(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
HEADING = re.compile(r"^## \[([^\]]+)\](?: - (\d{4}-\d{2}-\d{2}))?$", re.MULTILINE)
CATEGORIES = {"Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"}


def has_notes(body):
    bullets = re.findall(r"^- (.+)$", body, re.MULTILINE)
    return any(
        text.strip() and not re.match(r"(?i)^(todo|tbd|coming soon)\b", text.strip())
        for text in bullets
    )


def sections(text):
    headings = list(HEADING.finditer(text))
    if not headings or headings[0][1] != "Unreleased":
        raise ValueError("The changelog must start with ## [Unreleased].")
    if len(re.findall(r"^## ", text, re.MULTILINE)) != len(headings):
        raise ValueError("Use ## [vX.Y.Z] - YYYY-MM-DD for release headings.")
    result = {}
    for index, heading in enumerate(headings):
        name, date = heading[1], heading[2]
        if name in result:
            raise ValueError(f"Duplicate changelog entry: {name}")
        if name == "Unreleased":
            if date:
                raise ValueError("Unreleased must not have a release date.")
        else:
            if not re.fullmatch(r"v\d+\.\d+\.\d+(?:\.\d+)?(?:[-+][0-9A-Za-z.-]+)?", name):
                raise ValueError(f"Invalid historical release heading: {name}")
            if not date:
                raise ValueError(f"Missing release date for {name}.")
            datetime.date.fromisoformat(date)
        end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
        body = text[heading.end():end].strip()
        for category in re.findall(r"^### (.+)$", body, re.MULTILINE):
            if category not in CATEGORIES:
                raise ValueError(f"Unknown changelog category: {category}")
        if name != "Unreleased" and not has_notes(body):
            raise ValueError(f"{name} needs at least one non-placeholder change bullet.")
        result[name] = (heading.start(), end, body)
    return result


def workspace_version(root):
    return tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]


# Packages whose Cargo.lock entries carry the workspace version: the server
# workspace's members, and the desktop app's separate workspace (which also
# locks moonshine-management through its path dependency, so `--locked`
# builds fail when it is stale).
SERVER_LOCK = ("Cargo.lock", ["moonshine", "moonshine-core", "moonshine-management", "moonshine-tools"])
UI_LOCK = ("pyroshine-ui/src-tauri/Cargo.lock", ["moonshine-ui", "moonshine-management"])
UI_MANIFEST = "pyroshine-ui/src-tauri/Cargo.toml"
NPM_FILES = {
    "pyroshine-ui/package.json": [("version",)],
    "pyroshine-ui/package-lock.json": [("version",), ("packages", "", "version")],
}


def lock_entry(name):
    return re.compile(r'(\[\[package\]\]\nname = "' + re.escape(name) + r'"\nversion = ")([^"]*)(")')


def manifest_version():
    return re.compile(r'(\[package\]\n(?:[^\[\n][^\n]*\n|\n)*?version = ")([^"]*)(")')


def version_copies(root):
    """Every copy of the release version: (description, current value)."""
    copies = []
    for path, names in (SERVER_LOCK, UI_LOCK):
        text = (root / path).read_text()
        for name in names:
            match = lock_entry(name).search(text)
            copies.append((f"{path} ({name})", match[2] if match else None))
    match = manifest_version().search((root / UI_MANIFEST).read_text())
    copies.append((UI_MANIFEST, match[2] if match else None))
    for path, keys in NPM_FILES.items():
        document = json.loads((root / path).read_text())
        for key in keys:
            value = document
            for part in key:
                value = value.get(part) if isinstance(value, dict) else None
            copies.append((f"{path} ({'.'.join(part or '\"\"' for part in key)})", value))
    return copies


def version_mismatches(root):
    version = workspace_version(root)
    return [f"{where} is {value or 'missing'}" for where, value in version_copies(root) if value != version]


def sync_versions(root):
    """Write the workspace version to every copy; returns the changed files."""
    version = workspace_version(root)
    changed = []

    def write(path, text):
        if (root / path).read_text() != text:
            (root / path).write_text(text)
            changed.append(path)

    for path, names in (SERVER_LOCK, UI_LOCK):
        text = (root / path).read_text()
        for name in names:
            text, count = lock_entry(name).subn(lambda m: m[1] + version + m[3], text)
            if count != 1:
                raise ValueError(f"{path} has no single entry for {name}.")
        write(path, text)
    text, count = manifest_version().subn(lambda m: m[1] + version + m[3], (root / UI_MANIFEST).read_text(), count=1)
    if count != 1:
        raise ValueError(f"{UI_MANIFEST} has no [package] version.")
    write(UI_MANIFEST, text)
    for path, keys in NPM_FILES.items():
        document = json.loads((root / path).read_text())
        for key in keys:
            target = document
            for part in key[:-1]:
                target = target[part]
            target[key[-1]] = version
        # npm writes two-space indentation and a trailing newline.
        write(path, json.dumps(document, indent=2, ensure_ascii=False) + "\n")
    return changed


def release_notes(root, tag=None):
    version = workspace_version(root)
    expected = "v" + version
    tag = tag or expected
    if not SEMVER.fullmatch(tag):
        raise ValueError(f"New release tags must use SemVer, for example v1.2.3: {tag}")
    if tag != expected:
        raise ValueError(f"Release tag {tag} does not match Cargo.toml version {expected}.")
    stale = version_mismatches(root)
    if stale:
        raise ValueError(
            f"Version copies differ from the workspace version {version}: {'; '.join(stale)}. "
            "Run python3 scripts/changelog.py sync-versions and commit the result."
        )
    entries = sections((root / "docs/CHANGELOG.md").read_text())
    releases = [name for name in entries if name != "Unreleased"]
    if not releases or releases[0] != tag:
        raise ValueError(
            f"The latest dated changelog entry must be {tag}. "
            "Run python3 scripts/changelog.py prepare --date YYYY-MM-DD, "
            "then commit the changelog before tagging. The check command only validates entries."
        )
    return entries[tag][2] + "\n"


def prepare(root, date):
    date = datetime.date.fromisoformat(date).isoformat()
    version = workspace_version(root)
    tag = "v" + version
    if not SEMVER.fullmatch(tag):
        raise ValueError(f"Workspace version is not SemVer: {version}")
    path = root / "docs/CHANGELOG.md"
    text = path.read_text()
    entries = sections(text)
    if tag in entries:
        raise ValueError(f"{tag} already has an entry. Bump the workspace version first.")
    start, end, body = entries["Unreleased"]
    if not has_notes(body):
        raise ValueError("Add change bullets under Unreleased before preparing a release.")
    replacement = f"## [Unreleased]\n\n## [{tag}] - {date}\n\n{body}\n\n"
    updated = text[:start] + replacement + text[end:]
    sections(updated)
    sync_versions(root)
    path.write_text(updated)
    return tag


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="Check the current workspace version or a release tag")
    check.add_argument("--tag")
    notes = commands.add_parser("notes", help="Write the validated release entry to stdout")
    notes.add_argument("--tag", required=True)
    finalize = commands.add_parser("prepare", help="Move Unreleased notes to the workspace version")
    finalize.add_argument("--date", required=True, help="Release date in YYYY-MM-DD format")
    commands.add_parser("sync-versions", help="Write the workspace version to every other copy of it")
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            print(f"Prepared {prepare(ROOT, args.date)} in docs/CHANGELOG.md and synchronized versions; review and commit before tagging.")
        elif args.command == "sync-versions":
            changed = sync_versions(ROOT)
            print(f"Updated {', '.join(changed)}." if changed else f"All versions already match {workspace_version(ROOT)}.")
        else:
            body = release_notes(ROOT, args.tag)
            if args.command == "notes":
                sys.stdout.write(body)
            else:
                print("Changelog, release tag and every version copy agree with the workspace version.")
    except (ValueError, KeyError, OSError) as error:
        parser.exit(1, f"Changelog error: {error}\n")


if __name__ == "__main__":
    main()
