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


if __name__ == "__main__":
    unittest.main()
