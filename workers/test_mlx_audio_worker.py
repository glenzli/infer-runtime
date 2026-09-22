from __future__ import annotations

import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

import mlx_audio_worker as worker


class AudioToolResolutionTests(unittest.TestCase):
    def _tool_directory(self, root: Path) -> Path:
        directory = root / "audio-tools"
        directory.mkdir()
        for name in worker.REQUIRED_AUDIO_TOOLS:
            executable = directory / name
            executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            executable.chmod(0o700)
        return directory

    def test_uses_inherited_directory_containing_the_complete_tool_pair(self) -> None:
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ):
            directory = self._tool_directory(Path(temporary))
            inherited = os.pathsep.join(["/usr/bin", str(directory), "/bin"])

            resolved = worker._configure_audio_tools(
                inherited_path=inherited,
                configured_directory="",
                platform_directories=(),
            )

            self.assertEqual(resolved, directory)
            self.assertEqual(Path(os.environ["PATH"].split(os.pathsep)[0]), directory)

    def test_repairs_a_minimal_gui_path_from_a_platform_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ):
            directory = self._tool_directory(Path(temporary))

            resolved = worker._configure_audio_tools(
                inherited_path="/usr/bin:/bin:/usr/sbin:/sbin",
                configured_directory="",
                platform_directories=(directory,),
            )

            self.assertEqual(resolved, directory)
            self.assertTrue(os.environ["PATH"].startswith(f"{directory}{os.pathsep}"))

    def test_explicit_directory_is_authoritative_and_must_be_complete(self) -> None:
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ):
            root = Path(temporary)
            inherited = self._tool_directory(root)
            incomplete = root / "incomplete"
            incomplete.mkdir()
            (incomplete / "ffmpeg").write_text("ffmpeg", encoding="utf-8")
            (incomplete / "ffmpeg").chmod(0o700)

            with self.assertRaisesRegex(
                worker.AudioDependencyUnavailable,
                worker.FFMPEG_DIRECTORY_ENV,
            ):
                worker._configure_audio_tools(
                    inherited_path=str(inherited),
                    configured_directory=str(incomplete),
                    platform_directories=(),
                )

    def test_missing_tools_return_a_stable_dependency_error(self) -> None:
        with patch.dict(os.environ):
            with self.assertRaisesRegex(
                worker.AudioDependencyUnavailable,
                "audio_dependency_unavailable",
            ):
                worker._configure_audio_tools(
                    inherited_path="/system-only",
                    configured_directory="",
                    platform_directories=(),
                )

    def test_worker_returns_a_bounded_dependency_error_without_a_traceback(self) -> None:
        request = io.StringIO(
            '{"request_id":"request-1","operation":"transcribe"}\n'
        )
        response = io.StringIO()
        diagnostics = io.StringIO()
        error = worker.AudioDependencyUnavailable(
            "audio_dependency_unavailable: ffmpeg and ffprobe are required"
        )

        with (
            patch.object(worker, "_configure_audio_tools", side_effect=error),
            patch.object(worker.sys, "stdin", request),
            redirect_stdout(response),
            redirect_stderr(diagnostics),
        ):
            self.assertEqual(worker.main(), 0)

        self.assertEqual(diagnostics.getvalue(), "")
        self.assertEqual(
            json.loads(response.getvalue()),
            {
                "request_id": "request-1",
                "ok": False,
                "error": "audio_dependency_unavailable: ffmpeg and ffprobe are required",
            },
        )


class SpeechPaceTests(unittest.TestCase):
    def test_default_speed_does_not_transcode(self):
        with patch.object(worker.subprocess, "run") as run:
            worker._apply_qwen_pace(Path("narration.wav"), 1.0)
            run.assert_not_called()

    def test_tempo_is_bounded_and_applied_before_atomic_replacement(self):
        for speed in (0.25, 0.75, 1.25, 4.0):
            with patch.object(worker.subprocess, "run") as run, patch.object(worker.os, "replace") as replace:
                worker._apply_qwen_pace(Path("narration.wav"), speed)
                command = run.call_args.args[0]
                filters = command[command.index("-af") + 1].split(",")
                product = 1.0
                for item in filters:
                    factor = float(item.split("=")[1])
                    self.assertTrue(0.5 <= factor <= 2.0)
                    product *= factor
                self.assertAlmostEqual(product, speed)
                replace.assert_called_once()

    def test_failed_conversion_never_replaces_original(self):
        with patch.object(worker.subprocess, "run", side_effect=RuntimeError("failed")), patch.object(worker.os, "replace") as replace:
            with self.assertRaises(RuntimeError):
                worker._apply_qwen_pace(Path("narration.wav"), 1.25)
            replace.assert_not_called()


if __name__ == "__main__":
    unittest.main()
