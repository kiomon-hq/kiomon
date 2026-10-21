# SPDX-License-Identifier: Apache-2.0
"""Start (or find) a local brain and point the Python SDK at it.

This is the whole trick of running a published SDK against a local engine: the SDK defaults
to ``https://api.kiomon.com``, so you give it a ``base_url`` of ``127.0.0.1`` and an
``api_key`` that is the local token. Everything else - paths, bodies, error codes - is the
hosted contract, because the local server answers the hosted routes.

Two modes:

* **Nothing running** (the default here): start ``kiomon serve --port 0`` with a throwaway
  database, read the announcement line it prints, and load the token from the file beside
  that database. Both examples do this so they work from a clean checkout.
* **Something running**: set ``KIOMON_URL`` and ``KIOMON_TOKEN`` and the helper uses that
  instead. This is what you do against your real brain in another terminal.

Environment:

* ``KIOMON_BIN`` - the binary. Defaults to ``target/release/kiomon`` in this repository,
  then ``kiomon`` on ``PATH``.
* ``KIOMON_EXAMPLE_DB`` - where the throwaway brain lives. Defaults to a temp directory that
  is deleted on exit; point it at a real path to keep the brain and query it with the CLI
  afterwards.
"""

from __future__ import annotations

import contextlib
import json
import os
import subprocess
import sys
import tempfile
from collections.abc import Iterator
from pathlib import Path

# The SDK is a separate repository beside this one, and is also on PyPI. Try the installed
# package first - that is what a user following the README has - and fall back to the
# checkout so the examples run in this workspace with nothing installed.
try:
    from kiomon import Kiomon, KiomonError
except ImportError:  # pragma: no cover - the fallback is for this workspace only
    _SDK = Path(__file__).resolve().parents[4] / "SDK" / "kiomon-py" / "src"
    if not _SDK.is_dir():
        raise SystemExit(
            "the kiomon package is not installed and no SDK checkout was found beside this "
            "repository. Run `pip install kiomon`, or set PYTHONPATH to the SDK's src/."
        ) from None
    sys.path.insert(0, str(_SDK))
    from kiomon import Kiomon, KiomonError

__all__ = ["Kiomon", "KiomonError", "brain", "binary", "server_announcement"]


def binary() -> str:
    """The ``kiomon`` this example will drive: explicit, local build, then ``PATH``."""
    explicit = os.environ.get("KIOMON_BIN")
    if explicit:
        return explicit
    here = Path(__file__).resolve().parents[2]
    for name in ("kiomon", "kiomon.exe"):
        candidate = here / "target" / "release" / name
        if candidate.is_file():
            return str(candidate)
    return "kiomon"


def server_announcement(process: subprocess.Popen[str]) -> dict[str, object]:
    """Read the one JSON line ``serve --port 0 --json`` prints before it accepts requests.

    It is the first line of stdout, so this cannot race the server: the line is written
    before the socket is polled.
    """
    assert process.stdout is not None
    line = process.stdout.readline()
    if not line:
        raise SystemExit("`kiomon serve` exited before announcing a port; is the binary right?")
    return json.loads(line)


@contextlib.contextmanager
def brain() -> Iterator[tuple[Kiomon, str, Path]]:
    """Yield ``(client, url, database)`` - a running server and a client pointed at it."""
    url = os.environ.get("KIOMON_URL")
    token = os.environ.get("KIOMON_TOKEN")
    if url and token:
        yield Kiomon(api_key=token, base_url=url, workspace_id="default"), url, Path("<remote>")
        return

    keep = os.environ.get("KIOMON_EXAMPLE_DB")
    directory = Path(keep).parent if keep else Path(tempfile.mkdtemp(prefix="kiomon-example-"))
    directory.mkdir(parents=True, exist_ok=True)
    database = Path(keep) if keep else directory / "brain.db"

    environment = dict(os.environ, KIOMON_DB=str(database))
    process = subprocess.Popen(
        [binary(), "serve", "--port", "0", "--json"],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,  # `serve` narrates on stderr; the examples do their own
        text=True,
        env=environment,
    )
    try:
        announcement = server_announcement(process)
        url = str(announcement["url"])
        # The token file is created on first run and never rotated behind your back. Reading
        # it is the local equivalent of "copy your API key" - there is no key to issue.
        token = Path(str(announcement["token_file"])).read_text(encoding="utf-8").strip()
        print(f"  brain     {database}")
        print(f"  serve     {url}")
        yield Kiomon(api_key=token, base_url=url, workspace_id="default"), url, database
    finally:
        process.terminate()
        with contextlib.suppress(subprocess.TimeoutExpired):
            process.wait(timeout=5)
        if not keep:
            import shutil

            shutil.rmtree(directory, ignore_errors=True)
