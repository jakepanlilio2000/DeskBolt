#!/usr/bin/env python3
"""Exercise changelog generation with real commit ranges and release tags."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from update_changelog import git, update_changelog


class ChangelogTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.repo = Path(temp.name)
        git(self.repo, "init", "--quiet")
        git(self.repo, "config", "user.name", "Changelog Test")
        git(self.repo, "config", "user.email", "changelog@example.invalid")
        self.changelog = self.repo / "CHANGELOG.md"
        self.changelog.write_text("# Changelog\n", encoding="utf-8")
        self.commit("feat: initial setup")

    def commit(self, subject: str) -> str:
        git(self.repo, "commit", "--quiet", "--allow-empty", "-m", subject)
        return git(self.repo, "rev-parse", "--short", "HEAD").strip()

    def test_bootstrap_and_following_release_include_project_commits(self) -> None:
        self.commit("chore(release): bump version to v0.1.0-beta6 [skip ci]")
        update_changelog(self.changelog, "0.1.0-beta1", None)
        git(self.repo, "tag", "-a", "v0.1.0-beta1", "-m", "DeskBolt v0.1.0-beta1")
        fix = self.commit("fix: improve reconnection")
        docs = self.commit("docs: update setup instructions")
        update_changelog(self.changelog, "0.1.0-beta2", "0.1.0-beta1")

        log = self.changelog.read_text(encoding="utf-8")
        self.assertIn(f"- docs: update setup instructions (`{docs}`)", log)
        self.assertIn(f"- fix: improve reconnection (`{fix}`)", log)
        self.assertLess(log.index("## 0.1.0-beta2"), log.index("## 0.1.0-beta1"))
        self.assertEqual(log.count("feat: initial setup"), 1)
        self.assertNotIn("bump version", log)

    def test_duplicate_version_is_rejected(self) -> None:
        update_changelog(self.changelog, "0.1.0-beta1", None)
        with self.assertRaisesRegex(ValueError, "already contains"):
            update_changelog(self.changelog, "0.1.0-beta1", None)

    def test_missing_previous_tag_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "must be an ancestor"):
            update_changelog(self.changelog, "0.1.0-beta2", "0.1.0-beta1")
        self.assertEqual(self.changelog.read_text(encoding="utf-8"), "# Changelog\n")

    def test_empty_commit_range(self) -> None:
        git(self.repo, "tag", "v0.1.0-beta1")
        update_changelog(self.changelog, "0.1.0-beta2", "0.1.0-beta1")
        self.assertIn("No project changes.", self.changelog.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
