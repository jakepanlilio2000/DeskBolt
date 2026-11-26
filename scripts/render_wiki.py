#!/usr/bin/env python3
"""Render the public wiki from the repository and its latest published tag."""

import argparse
import re
from pathlib import Path

from bump_version import parse_version, read_workspace_version
from update_changelog import RELEASE_COMMIT, git


def section(readme: str, heading: str) -> str:
    start = f"## {heading}\n"
    if start not in readme:
        raise ValueError(f"README is missing {heading!r}")
    return readme.split(start, 1)[1].split("\n## ", 1)[0].strip()


def wiki_pages(repo: Path, url: str, tag: str) -> dict[str, str]:
    if not tag.startswith("v"):
        raise ValueError(f"Invalid release tag {tag!r}")
    parse_version(tag[1:])
    reference = f"refs/tags/{tag}"
    git(repo, "merge-base", "--is-ancestor", reference, "HEAD")
    readme = (repo / "README.md").read_text(encoding="utf-8")
    workspace = read_workspace_version(repo / "Cargo.toml")
    published = git(repo, "show", f"{reference}:CHANGELOG.md")
    fields = git(repo, "log", "-z", "--format=%h%x00%s", f"{reference}..HEAD").split("\0")
    pending = []
    for commit, subject in zip(fields[::2], fields[1::2]):
        if not RELEASE_COMMIT.fullmatch(subject):
            plain = re.sub(r"([\\<>&\[\]*_`])", r"\\\1", subject)
            pending.append(f"- {plain} ([{commit}]({url}/commit/{commit}))")
    changes = "\n".join(pending) or "The current source is covered by the latest release."
    return {
        "Home.md": (
            "DeskBolt is a self-hosted remote desktop, file transfer, and system administration suite.\n\n"
            f"**Latest published version** [{tag}]({url}/releases/tag/{tag})  \n"
            f"**Workspace version** `{workspace}`\n\n"
            f"[Setup and controls]({url}/wiki/Setup-and-Controls) · [Release history]({url}/wiki/Release-History)\n\n"
            f"## Core capabilities\n\n{section(readme, 'Core Capabilities')}\n"
        ),
        "Setup-and-Controls.md": (
            f"{section(readme, 'Quick Start')}\n\n"
            f"See the [developer reference]({url}#developer-reference) for source builds.\n"
        ),
        "Release-History.md": (
            f"## Commits since [{tag}]({url}/releases/tag/{tag})\n\n{changes}\n\n"
            f"{published.removeprefix('# Changelog').strip()}\n"
        ),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    pages = wiki_pages(Path.cwd(), args.url.rstrip("/"), args.tag)
    args.output.mkdir(parents=True, exist_ok=True)
    for name, content in pages.items():
        (args.output / name).write_text(content, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
