#!/usr/bin/env python3
"""Deterministic commit-driven workspace version bumper for DeskBolt."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)(?:-beta(\d+))?$")
BREAKING_HEADER_RE = re.compile(r"^[a-zA-Z]+(?:\([^)]+\))?!:")
FEAT_HEADER_RE = re.compile(r"^feat(?:\([^)]+\))?:")


def parse_version(version_str: str) -> tuple[int, int, int, int | None]:
    """Parses `X.Y.Z` or `X.Y.Z-betaN` into `(major, minor, patch, beta_or_none)`."""
    match = VERSION_RE.match(version_str.strip())
    if not match:
        raise ValueError(f"Unsupported version format: {version_str!r}")
    major, minor, patch = int(match.group(1)), int(match.group(2)), int(match.group(3))
    beta = int(match.group(4)) if match.group(4) is not None else None
    return major, minor, patch, beta


def compute_next_version(
    current_version: str,
    commit_message: str = "",
    bump: str = "auto",
) -> str:
    """Computes the next SemVer(+beta) version from commit tags or explicit `--bump`."""
    major, minor, patch, beta = parse_version(current_version)
    msg = commit_message.strip()
    msg_lower = msg.lower()
    first_line = msg.splitlines()[0].strip() if msg else ""

    has_overhaul = "[overhaul]" in msg_lower
    has_beta = "[beta]" in msg_lower
    has_stable = "[stable]" in msg_lower
    is_major_feat = (
        bool(BREAKING_HEADER_RE.match(first_line))
        or "[major]" in msg_lower
        or "breaking change" in msg_lower
    )
    is_minor_feat = (not is_major_feat) and bool(FEAT_HEADER_RE.match(first_line))

    if bump == "overhaul" or (bump == "auto" and has_overhaul):
        suffix = "-beta1" if has_beta else ""
        return f"{major + 1}.0.0{suffix}"

    if bump == "stable" or (bump == "auto" and has_stable):
        if is_major_feat:
            return f"{major}.{minor + 1}.0"
        if is_minor_feat:
            return f"{major}.{minor}.{patch + 1}"
        return f"{major}.{minor}.{patch}"

    if bump == "minor-beta" or (bump == "auto" and is_major_feat):
        return f"{major}.{minor + 1}.0-beta1"

    if bump == "feat-beta" or (bump == "auto" and is_minor_feat):
        return f"{major}.{minor}.{patch + 1}-beta1"

    if bump in ("auto", "patch-beta"):
        if beta is not None:
            return f"{major}.{minor}.{patch}-beta{beta + 1}"
        return f"{major}.{minor}.{patch + 1}-beta1"

    raise ValueError(f"Unknown bump mode: {bump!r}")


def read_workspace_version(manifest_path: Path) -> str:
    """Reads `[workspace.package] version` from `Cargo.toml`."""
    in_pkg = False
    for line in manifest_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_pkg = stripped == "[workspace.package]"
            continue
        if in_pkg:
            m = re.match(r'^version\s*=\s*"([^"]+)"', stripped)
            if m:
                return m.group(1)
    raise ValueError(f"[workspace.package] version not found in {manifest_path}")


def write_workspace_version(manifest_path: Path, next_version: str) -> None:
    """Updates `[workspace.package] version` in place without reformatting unrelated keys."""
    raw = manifest_path.read_text(encoding="utf-8")
    newline = "\r\n" if "\r\n" in raw else "\n"
    trailing_nl = raw.endswith(("\n", "\r\n"))

    lines = raw.splitlines()
    out_lines: list[str] = []
    in_pkg = False
    replaced = False

    for line in lines:
        stripped = line.strip()
        if stripped.startswith("["):
            in_pkg = stripped == "[workspace.package]"
            out_lines.append(line)
            continue
        if in_pkg and not replaced:
            m = re.match(r'^(\s*version\s*=\s*")[^"]+(".*)$', line)
            if m:
                out_lines.append(f"{m.group(1)}{next_version}{m.group(2)}")
                replaced = True
                continue
        out_lines.append(line)

    if not replaced:
        raise ValueError(f"Failed to update [workspace.package] version in {manifest_path}")

    updated = newline.join(out_lines) + (newline if trailing_nl else "")
    manifest_path.write_text(updated, encoding="utf-8")


def read_head_commit_message(repo_root: Path) -> str:
    res = subprocess.run(
        ["git", "log", "-1", "--pretty=%B"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
        timeout=15,
    )
    return res.stdout if res.returncode == 0 else ""


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Bump DeskBolt workspace version.")
    parser.add_argument("--manifest", type=Path, default=Path("Cargo.toml"))
    parser.add_argument("--message", type=str, default=None)
    parser.add_argument(
        "--bump",
        choices=("auto", "patch-beta", "feat-beta", "minor-beta", "stable", "overhaul"),
        default="auto",
    )
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    current = read_workspace_version(args.manifest)
    message = args.message
    if message is None and args.bump in ("auto", "stable", "overhaul"):
        message = read_head_commit_message(args.manifest.resolve().parent)

    next_ver = compute_next_version(current, message or "", args.bump)
    if not args.dry_run:
        write_workspace_version(args.manifest, next_ver)
    print(next_ver)
    return 0


if __name__ == "__main__":
    sys.exit(main())
