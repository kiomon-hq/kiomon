#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive `kiomon mcp` the way an MCP host does: JSON-RPC over stdin and stdout.

    python examples/mcp/session.py

No dependency, no SDK: this is the protocol, written out. It is also the shortest way to see
what an agent host receives, which is useful when a tool call "does nothing" in a client whose
logs you cannot read.

Two rules the protocol imposes and this file obeys:

* **stdout carries protocol frames only.** Everything a human should read goes to stderr, which
  is why `kiomon mcp` never prints a banner. A stray line on stdout is a client that hangs.
* **`initialize`, then `notifications/initialized`, then calls.** A request before the handshake
  is refused, and answering a notification is a desynchronised stream.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parents[2]


def binary() -> str:
    """The ``kiomon`` to drive: explicit, local build, then ``PATH``."""
    explicit = os.environ.get("KIOMON_BIN")
    if explicit:
        return explicit
    for name in ("kiomon", "kiomon.exe"):
        candidate = REPOSITORY / "target" / "release" / name
        if candidate.is_file():
            return str(candidate)
    return "kiomon"


class Session:
    """The smallest correct MCP client: one line out, one line back."""

    def __init__(self, process: subprocess.Popen[str]) -> None:
        self.process = process
        self.next_id = 1
        assert process.stdout is not None

    def notify(self, method: str, params: dict | None = None) -> None:
        """A notification has no id and takes no reply."""
        frame = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        self._write(frame)

    def request(self, method: str, params: dict | None = None) -> dict:
        """Send a request and read exactly one frame back."""
        request_id = self.next_id
        self.next_id += 1
        frame = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            frame["params"] = params
        self._write(frame)
        assert self.process.stdout is not None
        line = self.process.stdout.readline()
        if not line:
            raise SystemExit("the server closed stdout; its stderr says why")
        reply = json.loads(line)
        if reply.get("id") != request_id:
            raise SystemExit(f"out of order: asked for {request_id}, got {reply.get('id')}")
        return reply

    def call(self, tool: str, arguments: dict) -> tuple[str, bool]:
        """Call a tool and return ``(text, failed)``.

        The content is **rendered text, not JSON**: an MCP result is written for a model to
        read, so it says "1 drafted: 1 active, 0 waiting for review" and lists each memory as
        "- Title (id, status)". A host that needs structured ids should use the HTTP door
        (`serve`), where the same verb answers JSON. A failed tool is a *result* with
        `isError`, not a JSON-RPC error, so one refusal cannot desynchronise the stream.
        """
        reply = self.request("tools/call", {"name": tool, "arguments": arguments})
        if "error" in reply:
            raise SystemExit(f"{tool} failed at the protocol level: {reply['error']}")
        result = reply["result"]
        return result["content"][0]["text"], bool(result.get("isError"))

    def _write(self, frame: dict) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(frame) + "\n")
        self.process.stdin.flush()


def section(title: str) -> None:
    print("\n-- " + title + " " + "-" * max(0, 62 - len(title)))


def show(text: str) -> None:
    """Print a tool result the way it arrives: as text, one indented line at a time."""
    for line in text.splitlines():
        print("  " + line if line else "")


def main() -> None:
    # MCP has no token: the client owns the process, so stdio is the boundary. The database is
    # chosen by the environment or the config file, exactly as every other verb does it.
    directory = Path(tempfile.mkdtemp(prefix="kiomon-mcp-example-"))
    environment = dict(os.environ, KIOMON_DB=str(directory / "brain.db"))
    process = subprocess.Popen(
        [binary(), "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        # stderr is left alone on purpose. Merging it into stdout would corrupt the frame
        # stream, which is the failure this discipline exists to prevent -- and the diagnostics
        # are worth seeing, so they go to this script's own stderr.
        stderr=None,
        text=True,
        env=environment,
    )
    session = Session(process)
    try:
        section("Handshake")
        # The server answers with its protocol version, the `tools` capability and the agent
        # instructions -- the transport-level copy of the contract, so a client that never read
        # a README still gets the lifecycle.
        hello = session.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "examples/mcp/session.py", "version": "1.0.0"},
            },
        )
        print("  protocol  " + hello["result"]["protocolVersion"])
        print("  server    " + hello["result"]["serverInfo"]["name"])
        instructions = hello["result"].get("instructions", "")
        print("  brief     " + (instructions.splitlines()[0] if instructions else "(none)"))
        session.notify("notifications/initialized")

        section("What the agent can call")
        tools = session.request("tools/list")["result"]["tools"]
        print(f"  tools     {len(tools)}: " + ", ".join(tool["name"] for tool in tools))

        section("Save through the gate")
        # `draft_memory` is the write path. The same gate applies as everywhere else: an
        # autonomous workspace activates a confident reflection, a review-mode one does not.
        drafted, _ = session.call(
            "draft_memory",
            {
                "kind": "procedural",
                "title": "Roll back a bad deploy",
                "body": "Promote the previous tag, then run the migrations that tag expects.",
                "tags": ["deploy", "rollback"],
                "confidence": 0.9,
            },
        )
        show(drafted)

        section("Search, then retrieve")
        show(session.call("search_memory", {"query": "deploy", "limit": 5})[0])
        # `retrieve_memory` is the call an agent should prefer: the packet, not the ranking.
        show(session.call("retrieve_memory", {"intent": "execute", "query": "how do I roll back?"})[0])

        section("A refusal is a result, not a crash")
        # `revert` needs revision history this engine does not keep. The tool answers with
        # `isError` and a reason, so the agent can say "not available" instead of retrying --
        # and the stream stays in sync because the refusal was a result.
        text, failed = session.call("manage_memory", {"id": "k_anything", "action": "revert"})
        print(f"  isError   {failed}")
        show(text)
    finally:
        process.terminate()
        process.wait(timeout=5)
        shutil.rmtree(directory, ignore_errors=True)

    print("\nDone. agent-config.json is how a host is told to start this server.")


if __name__ == "__main__":
    main()
