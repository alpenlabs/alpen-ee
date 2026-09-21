"""Safety checks for runner-owned EEST Engine metadata files."""

import stat
import tempfile
import unittest
from pathlib import Path

from factories.alpen_client import _write_new_private_file


class EestFixtureFileTests(unittest.TestCase):
    def test_creates_owner_only_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "engine.jwt"

            _write_new_private_file(path, "secret\n", "Engine JWT secret")

            self.assertEqual(path.read_text(encoding="utf-8"), "secret\n")
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_refuses_to_replace_existing_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "engine.jwt"
            path.write_text("original\n", encoding="utf-8")

            with self.assertRaisesRegex(RuntimeError, "refusing to overwrite"):
                _write_new_private_file(path, "replacement\n", "Engine JWT secret")

            self.assertEqual(path.read_text(encoding="utf-8"), "original\n")

    def test_refuses_to_follow_existing_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            target.write_text("original\n", encoding="utf-8")
            path = Path(directory) / "engine.jwt"
            path.symlink_to(target)

            with self.assertRaisesRegex(RuntimeError, "refusing to overwrite"):
                _write_new_private_file(path, "replacement\n", "Engine JWT secret")

            self.assertEqual(target.read_text(encoding="utf-8"), "original\n")


if __name__ == "__main__":
    unittest.main()
