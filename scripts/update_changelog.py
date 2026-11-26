#!/usr/bin/env python3
"""Record project commits in the changelog for a DeskBolt release."""

from __future__ import annotations

import argparse
import re
import subprocess
from pathlib import Path

from bump_version import parse_version


RELEASE_COMMIT = re.compile(r"chore\(release\): bump version to v\S+ \[skip ci\]")


def git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=repo, check=True, capture_output=True,
        encoding="utf-8", timeout=15,
    ).stdout


def release_entries(repo: Path, previous_version: str | None) -> list[str]:
    revision = "HEAD"
    if previous_version is not None:
        previous_tag = f"refs/tags/v{previous_version}"
        try:
            git(repo, "merge-base", "--is-ancestor", previous_tag, "HEAD")
        except subprocess.CalledProcessError as error:
            raise ValueError(f"Previous release v{previous_version} must be an ancestor of HEAD") from error
        revision = f"{previous_tag}..HEAD"

    fields = git(repo, "log", "-z", "--format=%h%x00%s", revision).split("\0")
    return [
        f"- {subject} (`{commit}`)"
        for commit, subject in zip(fields[::2], fields[1::2])
        if not RELEASE_COMMIT.fullmatch(subject)
    ]


def update_changelog(changelog: Path, version: str, previous_version: str | None) -> None:
    current = changelog.read_text(encoding="utf-8")
    intro, separator, history = current.partition("\n## ")
    heading = f"## {version}"
    if heading in current.splitlines():
        raise ValueError(f"Changelog already contains {version}")

    entries = release_entries(changelog.parent, previous_version)
    section = "\n".join([heading, "", *entries]) if entries else f"{heading}\n\nNo project changes."
    previous_sections = f"{separator}{history}"
    changelog.write_text(f"{intro.rstrip()}\n\n{section}\n{previous_sections}", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--previous-version")
    args = parser.parse_args()
    try:
        parse_version(args.version)
        if args.previous_version is not None:
            parse_version(args.previous_version)
        update_changelog(Path("CHANGELOG.md"), args.version, args.previous_version)
    except ValueError as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
