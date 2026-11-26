# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for the retry and digest checks in corpus archive downloads."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest import mock
from urllib.error import HTTPError, URLError

from tools.measure import corpus_eval

ARCHIVE = b"pinned archive bytes"
URL = "https://example.invalid/project.tar.gz"
PIN = {"url": URL, "sha256": hashlib.sha256(ARCHIVE).hexdigest()}


class Response(io.BytesIO):
    """Return fixed bytes in the same way as an HTTP response."""


def respond(*outcomes):
    """Return a urlopen replacement that gives one outcome for each call."""
    queue = list(outcomes)

    def fake_urlopen(url, timeout):
        outcome = queue.pop(0)
        if isinstance(outcome, Exception):
            raise outcome
        return Response(outcome)

    return mock.Mock(side_effect=fake_urlopen)


class CachedArchiveTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.cache = Path(tmp.name)
        self.sleep = mock.Mock()
        for patcher in (
            mock.patch.object(corpus_eval, "CACHE", self.cache),
            mock.patch.object(corpus_eval.time, "sleep", self.sleep),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def fetch(self, urlopen):
        stderr = io.StringIO()
        with mock.patch.object(corpus_eval, "urlopen", urlopen), redirect_stdout(io.StringIO()), redirect_stderr(stderr):
            try:
                return corpus_eval.cached_archive(PIN), stderr.getvalue()
            except BaseException as error:
                error.stderr = stderr.getvalue()
                raise

    def assert_no_partial_files(self, keep=()):
        self.assertEqual(sorted(path.name for path in self.cache.iterdir()), sorted(keep))

    def test_retries_after_html_error_page_then_succeeds(self):
        urlopen = respond(b"<html>Service unavailable</html>", ARCHIVE)
        archive, stderr = self.fetch(urlopen)
        self.assertEqual(archive.read_bytes(), ARCHIVE)
        self.assertEqual(urlopen.call_count, 2)
        self.sleep.assert_called_once_with(1)
        self.assertIn("Download attempt 1 failed: SHA-256 mismatch", stderr)
        self.assert_no_partial_files(keep=[archive.name])

    def test_retries_after_network_errors_then_succeeds(self):
        urlopen = respond(URLError("connection reset"), TimeoutError("read timed out"), ARCHIVE)
        archive, _ = self.fetch(urlopen)
        self.assertEqual(archive.read_bytes(), ARCHIVE)
        self.assertEqual([call.args for call in self.sleep.call_args_list], [(1,), (3,)])

    def test_persistent_mismatch_fails_after_three_attempts(self):
        urlopen = respond(b"truncated", b"truncated", b"truncated")
        with self.assertRaises(corpus_eval.DigestMismatch) as caught:
            self.fetch(urlopen)
        self.assertEqual(urlopen.call_count, 3)
        self.assertEqual(self.sleep.call_count, 2)
        self.assertIn("Download attempt 2 failed", caught.exception.stderr)
        self.assert_no_partial_files()

    def test_client_error_is_not_retried(self):
        not_found = HTTPError(URL, 404, "Not Found", {}, None)
        urlopen = respond(not_found)
        with self.assertRaises(HTTPError):
            self.fetch(urlopen)
        self.assertEqual(urlopen.call_count, 1)
        self.sleep.assert_not_called()

    def test_server_error_is_retried(self):
        unavailable = HTTPError(URL, 503, "Service Unavailable", {}, None)
        archive, _ = self.fetch(respond(unavailable, ARCHIVE))
        self.assertEqual(archive.read_bytes(), ARCHIVE)
        self.sleep.assert_called_once_with(1)

    def test_valid_cache_entry_skips_download(self):
        (self.cache / f"{PIN['sha256']}.tar.tmp").write_bytes(ARCHIVE)
        urlopen = respond()
        archive, _ = self.fetch(urlopen)
        self.assertEqual(archive.read_bytes(), ARCHIVE)
        urlopen.assert_not_called()


if __name__ == "__main__":
    unittest.main()
