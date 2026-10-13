import importlib.util
import unittest
from pathlib import Path

import numpy as np


SPEC = importlib.util.spec_from_file_location(
    "coreml_sam_worker", Path(__file__).with_name("coreml_sam_worker.py")
)
WORKER = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(WORKER)


class SamPromptContractTests(unittest.TestCase):
    def test_float32_edge_box_survives_worker_json_decoding(self) -> None:
        # These shortest decimal f32 values sum to 1 in the public Rust
        # contract, but their Python float64 sum is 1.000000003.
        start, extent = 0.045117203, 0.9548828
        self.assertGreater(start + extent, 1.0)
        self.assertEqual(np.float32(start) + np.float32(extent), 1.0)
        for axis in ("x", "y"):
            with self.subTest(axis=axis):
                box = {"x": 0.1, "y": 0.1, "width": 0.8, "height": 0.8}
                box[axis] = start
                box["width" if axis == "x" else "height"] = extent
                points, labels = WORKER._prompt_arrays(
                    {"points": [], "box_prompt": box}
                )
                self.assertEqual(points[0, 1, 0 if axis == "x" else 1], 1024.0)
                self.assertTrue(np.all(points >= 0.0))
                self.assertTrue(np.all(points <= 1024.0))
                np.testing.assert_array_equal(labels, [[2, 3]])

    def test_actual_out_of_bounds_and_nonfinite_boxes_stay_invalid(self) -> None:
        valid = {"x": 0.045117203, "y": 0.1, "width": 0.9548828, "height": 0.8}
        for field, value in (
            ("width", 0.9548848),
            ("height", 0.900001),
            ("x", -0.000001),
            ("width", 0.0),
            ("height", -0.1),
            ("x", float("nan")),
            ("height", float("inf")),
        ):
            with self.subTest(field=field, value=value):
                with self.assertRaises(WORKER.StableWorkerError) as raised:
                    WORKER._prompt_arrays(
                        {"points": [], "box_prompt": {**valid, field: value}}
                    )
                self.assertEqual(raised.exception.code, "sam_invalid_request")

    def test_point_and_edge_box_keep_the_model_prompt_order(self) -> None:
        coordinates, labels = WORKER._prompt_arrays(
            {
                "points": [{"x": 0.5, "y": 0.5, "label": "foreground"}],
                "box_prompt": {
                    "x": 0.1,
                    "y": 0.045117203,
                    "width": 0.8,
                    "height": 0.9548828,
                },
            }
        )
        self.assertEqual(coordinates.shape, (1, 3, 2))
        np.testing.assert_array_equal(coordinates[0, 0], [512.0, 512.0])
        self.assertEqual(coordinates[0, 2, 1], 1024.0)
        np.testing.assert_array_equal(labels, [[1, 2, 3]])


if __name__ == "__main__":
    unittest.main()
