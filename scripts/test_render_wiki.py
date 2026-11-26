"""Verify wiki output against real published tags and subsequent commits."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from render_wiki import wiki_pages
from update_changelog import git


class WikiTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.repo = Path(temp.name)
        self.url = "https://github.com/example/DeskBolt"
        git(self.repo, "init", "--quiet")
        git(self.repo, "config", "user.name", "Wiki Test")
        git(self.repo, "config", "user.email", "wiki@example.invalid")
        self.write("Cargo.toml", '[workspace.package]\nversion = "1.0.0-beta1"\n')
        self.write("README.md", "## Core Capabilities\n\n- Clipboard sync\n\n## Quick Start\n\nConnect a host.\n")
        self.write("CHANGELOG.md", "# Changelog\n\n## 1.0.0-beta1\n\n- Initial release\n")
        self.commit("feat: initial release")
        git(self.repo, "tag", "v1.0.0-beta1")

    def write(self, path: str, content: str) -> None:
        (self.repo / path).write_text(content, encoding="utf-8")

    def commit(self, subject: str) -> str:
        git(self.repo, "add", "--", "Cargo.toml", "README.md", "CHANGELOG.md")
        git(self.repo, "commit", "--quiet", "--allow-empty", "-m", subject)
        return git(self.repo, "rev-parse", "--short", "HEAD").strip()

    def test_published_history_and_later_commits_stay_current(self) -> None:
        initial = wiki_pages(self.repo, self.url, "v1.0.0-beta1")
        self.assertIn("current source is covered", initial["Release-History.md"])
        self.write("Cargo.toml", '[workspace.package]\nversion = "1.0.0-beta2"\n')
        self.write("CHANGELOG.md", "# Changelog\n\n## 1.0.0-beta2\n\n- Candidate notes\n")
        self.write("README.md", "## Core Capabilities\n\n- File sync\n\n## Quick Start\n\n### Linux\n\nOpen the viewer.\n")
        first = self.commit("feat: add file sync")
        second = self.commit("docs: describe [Linux] setup")
        self.commit("chore(release): bump version to v1.0.0-beta2 [skip ci]")
        pages = wiki_pages(self.repo, self.url, "v1.0.0-beta1")
        self.assertIn("releases/tag/v1.0.0-beta1", pages["Home.md"])
        self.assertIn("**Workspace version** `1.0.0-beta2`", pages["Home.md"])
        self.assertIn("File sync", pages["Home.md"])
        self.assertIn("### Linux", pages["Setup-and-Controls.md"])
        history = pages["Release-History.md"]
        self.assertIn(f"{self.url}/commit/{first}", history)
        self.assertIn(f"{self.url}/commit/{second}", history)
        self.assertIn(r"\[Linux\]", history)
        self.assertIn("Initial release", history)
        self.assertNotIn("Candidate notes", history)
        self.assertNotIn("bump version", history)


if __name__ == "__main__":
    unittest.main()
