#!/usr/bin/env python3
"""Unit tests for `scripts/bump_version.py`."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from bump_version import (
    compute_next_version,
    main,
    read_workspace_version,
    write_workspace_version,
)


class BumpVersionTests(unittest.TestCase):
    def test_required_progression_chain(self) -> None:
        v = "0.1.0-beta1"
        v = compute_next_version(v, "fix: prevent frame drop under backpressure")
        self.assertEqual(v, "0.1.0-beta2")

        v = compute_next_version(v, "feat(viewer): add copy address menu item")
        self.assertEqual(v, "0.1.1-beta1")

        v = compute_next_version(v, "feat!: add cross-platform capture trait")
        self.assertEqual(v, "0.2.0-beta1")

        v = compute_next_version(v, "chore(release): promote desktop build [stable]")
        self.assertEqual(v, "0.2.0")

        v = compute_next_version(v, "refactor: wire protocol v2 [overhaul]")
        self.assertEqual(v, "1.0.0")

        v = compute_next_version(v, "feat!: redesign file sync channel")
        self.assertEqual(v, "1.1.0-beta1")

    def test_edge_cases_and_tag_combinations(self) -> None:
        self.assertEqual(
            compute_next_version("0.2.3-beta2", "rewrite engine [overhaul] [beta]"),
            "1.0.0-beta1",
        )
        self.assertEqual(
            compute_next_version("1.4.0", "next architecture [overhaul]"),
            "2.0.0",
        )
        self.assertEqual(
            compute_next_version("0.1.2-beta3", "chore: [major] update crypto suite"),
            "0.2.0-beta1",
        )
        self.assertEqual(
            compute_next_version("0.1.2-beta3", "feat: new framing\n\nBREAKING CHANGE: header"),
            "0.2.0-beta1",
        )
        self.assertEqual(
            compute_next_version("1.0.0", "fix: clipboard crash on lock screen"),
            "1.0.1-beta1",
        )
        self.assertEqual(
            compute_next_version("1.0.0", "feat: display selector"),
            "1.0.1-beta1",
        )
        self.assertEqual(
            compute_next_version("0.2.0-beta1", "feat: adaptive fps [stable]"),
            "0.2.1",
        )
        self.assertEqual(
            compute_next_version("0.1.2-beta3", "feat!: overhaul auth [stable]"),
            "0.2.0",
        )

    def test_explicit_bump_modes(self) -> None:
        self.assertEqual(compute_next_version("0.1.0-beta1", "", "patch-beta"), "0.1.0-beta2")
        self.assertEqual(compute_next_version("0.1.0", "", "patch-beta"), "0.1.1-beta1")
        self.assertEqual(compute_next_version("0.1.0-beta2", "", "feat-beta"), "0.1.1-beta1")
        self.assertEqual(compute_next_version("0.1.1-beta1", "", "minor-beta"), "0.2.0-beta1")
        self.assertEqual(compute_next_version("0.2.0-beta3", "", "stable"), "0.2.0")
        self.assertEqual(compute_next_version("0.2.0-beta3", "", "overhaul"), "1.0.0")

    def test_toml_in_place_update_preserves_formatting(self) -> None:
        sample = (
            "# Header comment\n"
            "[workspace]\n"
            'resolver = "2"\n\n'
            "[workspace.package]\n"
            'version = "0.1.0-beta1"\n'
            'edition = "2024"\n'
            'license = "MPL-2.0"\n\n'
            "[workspace.dependencies]\n"
            'tokio = { version = "1.54.1", features = ["full"] }\n'
        )
        with tempfile.TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "Cargo.toml"
            manifest.write_text(sample, encoding="utf-8")
            self.assertEqual(read_workspace_version(manifest), "0.1.0-beta1")
            write_workspace_version(manifest, "0.1.1-beta1")
            updated = manifest.read_text(encoding="utf-8")
            self.assertEqual(read_workspace_version(manifest), "0.1.1-beta1")
            self.assertIn('tokio = { version = "1.54.1", features = ["full"] }', updated)
            self.assertIn("# Header comment\n", updated)
            self.assertEqual(main(["--manifest", str(manifest), "--bump", "stable"]), 0)
            self.assertEqual(read_workspace_version(manifest), "0.1.1")


if __name__ == "__main__":
    unittest.main()
