"""Checks public limits and exact decoder boundaries without loading weights."""
import importlib.util
import math
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("gemma_worker", Path(__file__).with_name("embeddinggemma2_worker.py"))
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class GemmaWorkerTests(unittest.TestCase):
    def test_invalid_query_rejected_before_loading(self):
        with patch.object(worker, "load_model") as load:
            for text in ("", "   ", "海" * 5462):
                with self.assertRaises(worker.InputError):
                    worker.handle({"kind": "text", "text": text, "model": "absent"}, "/absent")
            load.assert_not_called()

    def test_vector_cannot_claim_validity_with_nan_zero_or_wrong_dimension(self):
        worker.validate_vector([1.0] + [0.0] * 767)
        for values in ([0.0] * 768, [1.0], [math.nan] + [0.0] * 767):
            with self.assertRaises(RuntimeError):
                worker.validate_vector(values)

    def test_duration_and_modality_are_checked_before_decoding(self):
        import json
        def reply(seconds, kind="audio"):
            return SimpleNamespace(returncode=0, stdout=json.dumps({"format": {"duration": seconds}, "streams": [{"codec_type": kind, "width": 1920, "height": 1080}]}).encode())
        with patch.object(worker.subprocess, "run", return_value=reply(30)):
            self.assertEqual(worker.probe_media("private", "ffprobe", "audio"), 30)
        for result, kind in ((reply(30.001), "audio"), (reply(16.001, "video"), "video"), (reply(1, "video"), "audio"), (reply("nan"), "audio")):
            with patch.object(worker.subprocess, "run", return_value=result):
                with self.assertRaises(worker.InputError):
                    worker.probe_media("private", "ffprobe", kind)

    def test_image_rejects_arbitrary_file_before_loading(self):
        with tempfile.NamedTemporaryFile() as file:
            file.write(b"not an image")
            file.flush()
            with patch.object(worker, "load_model") as load:
                with self.assertRaises(Exception):
                    worker.handle({"kind": "image", "file": file.name}, "/absent")
                load.assert_not_called()


if __name__ == "__main__":
    unittest.main()
