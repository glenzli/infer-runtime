"""Read-only boundary and preservation checks using synthetic bridge objects."""
from pathlib import Path
import tempfile
import unittest

from publish_windows_bridge import publish, reject_links, validate_layout


class BridgePublicationTests(unittest.TestCase):
    def test_merge_replaces_only_the_owned_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, destination = root / "stage", root / "actual"
            source.mkdir()
            destination.mkdir()
            (source / "credentials").mkdir()
            (destination / "credentials").mkdir()
            (source / "credentials/infer-runtime-shadow.token").write_text("new synthetic token")
            (destination / "credentials/infer-runtime-shadow.token").write_text("old synthetic token")
            (destination / "unrelated-user-record").write_text("preserve")
            publish(source, destination)
            self.assertEqual((destination / "credentials/infer-runtime-shadow.token").read_text(), "new synthetic token")
            self.assertEqual((destination / "unrelated-user-record").read_text(), "preserve")
            self.assertFalse(source.exists())

    def test_unexpected_payload_is_rejected_before_a_directory_move(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Shadow").mkdir()
            (root / "Shadow/unrelated-catalog").write_text("keep in staging")
            with self.assertRaises(ValueError):
                validate_layout(root)
            self.assertTrue((root / "Shadow/unrelated-catalog").is_file())

    def test_linked_destination_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "actual").mkdir()
            (root / "alias").symlink_to(root / "actual", target_is_directory=True)
            with self.assertRaises(ValueError):
                reject_links(root / "alias/credentials")


if __name__ == "__main__":
    unittest.main()
