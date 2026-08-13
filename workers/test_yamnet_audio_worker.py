import importlib.util
import unittest
from pathlib import Path

import numpy as np


MODULE_PATH = Path(__file__).with_name("yamnet_audio_worker.py")
SPEC = importlib.util.spec_from_file_location("yamnet_audio_worker", MODULE_PATH)
WORKER = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(WORKER)


class YamnetPolicyTests(unittest.TestCase):
    def test_events_use_audioset_mid_and_bounded_temporal_intervals(self) -> None:
        scores = np.zeros((4, WORKER.CLASS_COUNT), dtype=np.float32)
        scores[1:3, 42] = 0.8
        classes = [(f"/m/test{index}", f"Class {index}") for index in range(WORKER.CLASS_COUNT)]
        events = WORKER._events(scores, classes, 2.0)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["class_id"], "/m/test42")
        self.assertEqual(events[0]["start_seconds"], 0.48)
        self.assertEqual(events[0]["end_seconds"], 1.92)
        self.assertEqual(events[0]["score"], 0.8)

    def test_speech_absence_comes_from_low_scores_not_empty_events(self) -> None:
        low = np.zeros((4, WORKER.CLASS_COUNT), dtype=np.float32)
        evidence = WORKER._speech_presence(low)
        self.assertEqual(evidence["status"], "absent")

        uncertain = low.copy()
        uncertain[:, 0] = 0.12
        self.assertEqual(WORKER._speech_presence(uncertain)["status"], "unknown")

        present = low.copy()
        present[:, 0] = 0.42
        self.assertEqual(WORKER._speech_presence(present)["status"], "present")

    def test_policy_bounds_each_window_to_the_highest_scoring_classes(self) -> None:
        scores = np.zeros((1, WORKER.CLASS_COUNT), dtype=np.float32)
        scores[0, : WORKER.MAX_CLASSES_PER_WINDOW + 5] = np.linspace(
            0.2, 0.9, WORKER.MAX_CLASSES_PER_WINDOW + 5
        )
        classes = [(f"/m/test{index}", f"Class {index}") for index in range(WORKER.CLASS_COUNT)]
        events = WORKER._events(scores, classes, 0.96)
        self.assertEqual(len(events), WORKER.MAX_CLASSES_PER_WINDOW)


if __name__ == "__main__":
    unittest.main()
