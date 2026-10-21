#!/usr/bin/env python3
"""The SDK conformance run: the real client, the real transport, a live `kiomon serve`.

The plan's WP9 acceptance criterion was "the two published SDKs' test suites pass against
`127.0.0.1`". Those suites cannot satisfy it: they drive a `FakeTransport`, so they prove the
client's planning and never open a socket. What is left to prove is one layer down and one
layer over — that the server a `FakeTransport` stands in for answers the shapes the SDK
declares in `types.py` / `types.ts`. This script is that proof, and it is written the way a
user would use the SDK: construct it, call the methods, read the fields.

Run it against a build:

    python3 scripts/sdk_conformance.py --bin target/release/kiomon \
        --sdk ../SDK/kiomon-py/src

It starts its own `serve` on `--port 0`, writes into a temporary directory, and exits non-zero
with a list of divergences. Every check names the SDK field it is standing on, so a failure
reads as "the client expects X" rather than "assert failed".
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

FAILED: list[str] = []


def check(condition: bool, what: str) -> None:
    """Record a divergence instead of raising, so one run reports all of them."""
    if not condition:
        FAILED.append(what)


def typed(value: object, name: str, python_type: type | tuple[type, ...]) -> None:
    """One field against the type the SDK's TypedDict declares for it."""
    check(
        isinstance(value, python_type),
        f"{name} should be {python_type}, got {type(value).__name__} ({value!r})",
    )


def typed_int(value: object, name: str) -> None:
    """An integer field, with `bool` refused.

    Python's `isinstance(True, int)` is `True`, so a `pinned: false` in the JSON would sail
    through a plain `int` check while breaking every typed client in a language where the two
    are distinct — which is most of them, starting with the one whose row this mirrors.
    """
    check(
        isinstance(value, int) and not isinstance(value, bool),
        f"{name} should be an integer, got {type(value).__name__} ({value!r})",
    )


def required(body: dict, fields: list[str], where: str) -> None:
    """Every field the SDK marks required on a response object."""
    for field in fields:
        check(field in body, f"{where} is missing `{field}`, which the SDK declares required")


def environment(home: Path) -> dict[str, str]:
    """A brain and a config file of our own, so the run never touches the user's.

    The config file is written empty rather than pointed at a missing path: a *named* config
    that does not exist is a hard error here (correctly — somebody asked for that file), so
    the harness provides one and keeps the user's `~/.kiomon/config.toml` out of the run. An
    embedder configured there would make the vector leg run and change every answer.
    """
    config = home / "config.toml"
    if not config.exists():
        config.write_text("", encoding="utf-8")
    return dict(os.environ, KIOMON_DB=str(home / "brain.db"), KIOMON_CONFIG=str(config))


def start_server(binary: Path, home: Path, port: int = 0) -> tuple[str, str, subprocess.Popen]:
    """Bind a server on an ephemeral port and return (base_url, token, process)."""
    env = environment(home)
    proc = subprocess.Popen(
        [str(binary), "serve", "--port", str(port), "--json", "-q"],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        env=env,
    )
    assert proc.stdout is not None
    line = proc.stdout.readline()
    if not line.strip():
        proc.kill()
        raise SystemExit("serve printed no handshake")
    url = json.loads(line)["url"]
    # The SDK demands a credential; a local server ignores its value. `local` is the
    # documented placeholder rather than a secret, so there is nothing to read from disk and
    # nothing to keep in sync when the server restarts.
    token = "local"
    return url, token, proc


def seed(binary: Path, home: Path) -> None:
    """Two memories and one edge: enough for every read to have something to say."""
    env = environment(home)
    bodies = [
        ("semantic", "Staging deploys need a manual migration", "Run migrations before deploying to staging.", ["deploy", "staging"]),
        ("procedural", "How to deploy staging", "Migrate, then deploy: kiomon deploy --env staging.", ["deploy", "routine"]),
    ]
    for kind, title, body, tags in bodies:
        args = [str(binary), "add", "--kind", kind, "--title", title, "--body", body, "--confidence", "0.9"]
        for tag in tags:
            args += ["--tag", tag]
        subprocess.run([*args, "-q"], check=True, env=env, capture_output=True, text=True)


def probe(url: str, token: str, path: str) -> dict:
    """The raw wire, for the refusals a well-behaved SDK would hide."""
    request = urllib.request.Request(url + path, headers={"X-API-Key": token})
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return {"status": response.status, "body": json.loads(response.read() or b"{}")}
    except urllib.error.HTTPError as error:
        return {"status": error.code, "body": json.loads(error.read() or b"{}")}


def run(binary: Path, sdk_src: Path) -> int:
    sys.path.insert(0, str(sdk_src))
    try:
        from kiomon import Kiomon
        from kiomon.errors import KiomonError
    except ImportError as error:
        print(f"cannot import the SDK from {sdk_src}: {error}", file=sys.stderr)
        return 2

    home = Path(tempfile.mkdtemp(prefix="kiomon-conformance-"))
    try:
        seed(binary, home)
        url, token, proc = start_server(binary, home)
        try:
            client = Kiomon(api_key=token, base_url=url)

            # ── workspaces: both SDKs declare user_id and slug as required ────────
            listed = client.list_workspaces()
            required(listed, ["workspaces"], "GET /api/workspaces")
            workspaces = listed["workspaces"]
            check(len(workspaces) == 1, f"expected one local workspace, got {len(workspaces)}")
            if workspaces:
                workspace = workspaces[0]
                required(
                    workspace,
                    ["id", "user_id", "name", "slug", "created_at"],
                    "Workspace",
                )
                typed(workspace.get("id"), "Workspace.id", str)
                typed(workspace.get("user_id"), "Workspace.user_id", str)
                typed(workspace.get("slug"), "Workspace.slug", str)
            # `select_workspace` is how a user pins the default, and `pick_workspace` is what
            # the SDK does when they do not: exactly one workspace resolves silently.
            client = Kiomon(api_key=token, base_url=url, workspace_id=workspace["id"])

            # ── search: `results`, then one row against SearchHit ────────────────
            found = client.search_memory(query="deploy", limit=5)
            required(found, ["results"], "GET /api/search")
            check(len(found["results"]) == 2, f"expected both seeded rows, got {len(found['results'])}")
            if found["results"]:
                hit = found["results"][0]
                required(
                    hit,
                    ["id", "title", "source_url", "excerpt", "snippet", "tags", "created_at"],
                    "SearchHit",
                )
                # The comment in the TypeScript SDK's types.ts is the whole reason this is
                # checked: "tags and headings arrive as JSON strings, not arrays".
                typed(hit.get("tags"), "SearchHit.tags (a JSON string, per the SDK)", str)
                typed(hit.get("source_url"), "SearchHit.source_url", str)
                typed(hit.get("snippet"), "SearchHit.snippet", str)
                typed(hit.get("created_at"), "SearchHit.created_at", str)
                try:
                    tags = json.loads(hit["tags"])
                    check(isinstance(tags, list), "SearchHit.tags must decode to a list")
                except (TypeError, ValueError) as error:
                    FAILED.append(f"SearchHit.tags is not decodable JSON: {error}")

                # ── get_memory: the hosted Document, field for field ─────────────
                document = client.get_memory(hit["id"])
                required(
                    document,
                    [
                        "id", "source_url", "title", "author", "excerpt", "tags", "user_notes",
                        "body", "pinned", "created_at", "workspace_id", "source", "kind",
                        "confidence", "corroboration_count", "importance", "strength",
                        "retrieval_count", "last_retrieved_at", "feedback_score",
                        "feedback_count", "scope", "status", "expires_at",
                    ],
                    "MemoryDocument",
                )
                # `pinned` is an integer in the hosted row and in both SDKs, and the PATCH
                # that writes it sends 1/0 — a bool here would round-trip as a failure.
                typed_int(document.get("pinned"), "MemoryDocument.pinned")
                typed(document.get("tags"), "MemoryDocument.tags", str)
                typed(document.get("source_url"), "MemoryDocument.source_url", str)
                typed(document.get("scope"), "MemoryDocument.scope", str)
                typed(document.get("body"), "MemoryDocument.body", str)
                check(
                    "author" in document and "user_notes" in document,
                    "MemoryDocument should carry `author` and `user_notes`, even as null",
                )

                # ── pin/unpin: the PATCH body the SDK actually sends is {"pinned": 1} ─
                pinned = client.manage_memory(id=hit["id"], action="pin")
                check(pinned is not None, "manage_memory('pin') returned no body")
                after = client.get_memory(hit["id"])
                check(after.get("pinned") == 1, f"pinned should be 1 after a pin, got {after.get('pinned')!r}")
                client.manage_memory(id=hit["id"], action="unpin")

                # ── rate: a write whose reply the SDK does not read but must not fail ─
                client.manage_memory(id=hit["id"], action="rate", value=1)

                # ── graph: centre_id, and the subgraph envelope ──────────────────
                graph = client.explore_memory_graph(id=hit["id"])
                required(graph, ["nodes", "edges"], "GraphResult")
                check(len(graph["nodes"]) >= 1, "the graph answer should contain the centre node")
                check(
                    all(isinstance(node, dict) and "id" in node for node in graph["nodes"]),
                    "every graph node should carry an id",
                )
                check("entityLinks" in graph, "GraphResult should carry `entityLinks`, even empty")

                # ── links: the hosted route the plan's appendix had missed ───────
                raw_links = probe(url, token, f"/api/documents/{hit['id']}/links")
                check(raw_links["status"] == 200, f"GET links answered {raw_links['status']}")
                check(isinstance(raw_links["body"], list), "GET links should answer an array of edges")

                # ── the packet: every required ContextPacket field ───────────────
                packet = client.retrieve_memory(intent="execute", query="deploy staging")
                required(
                    packet,
                    ["summary", "facts", "procedure", "episodes", "evidence", "warnings", "conflicts", "retrieved_ids"],
                    "ContextPacket",
                )
                for name in ("facts", "episodes", "evidence", "warnings", "conflicts", "retrieved_ids"):
                    typed(packet.get(name), f"ContextPacket.{name}", list)
                if packet["procedure"]:
                    required(
                        packet["procedure"],
                        ["memory_id", "kind", "claim", "confidence", "authority", "applicability"],
                        "PacketItem",
                    )

                # ── record_outcome: the write the packet's ids feed ──────────────
                outcome = client.record_outcome(
                    outcome="success",
                    items=[{"memory_id": mid, "assessment": "helpful"} for mid in packet["retrieved_ids"][:1]],
                )
                check(outcome is not None, "record_outcome returned no body")

                # ── batch_draft: the write gate, and its three lists ─────────────
                drafted = client.reflect_session(
                    learnings=[
                        {
                            "kind": "semantic",
                            "title": "Conformance run wrote this",
                            "body": "A learning submitted by scripts/sdk_conformance.py.",
                            "confidence": 0.9,
                            "tags": ["conformance"],
                            "source_refs": [hit["id"]],
                            # The three fields that skip a paid extraction pass. Both SDKs
                            # document them and both send them, and the local server refused
                            # them for a day because nothing in this harness ever wrote with
                            # them: a suite is only as wide as the calls it makes.
                            "entities": ["worker"],
                            "claims": ["a conformance run should be able to write"],
                            "domain": "conformance",
                        }
                    ]
                )
                required(drafted, ["drafted", "promoted", "pending", "memories"], "BatchDraftResult")
                typed(drafted.get("drafted"), "BatchDraftResult.drafted", int)
                if drafted["memories"]:
                    required(drafted["memories"][0], ["id", "title", "kind"], "BatchDraftMemory")
                # The envelope the writer sent must survive: if `entities` were silently
                # dropped rather than refused, that is still a broken write.
                written = client.get_memory(drafted["memories"][0]["id"])
                if written.get("memory_metadata"):
                    check(
                        "worker" in (written["memory_metadata"].get("entities") or []),
                        "the entities a draft declared were not stored",
                    )

            # ── context: the orientation read, shape server-defined ──────────────
            context = client.get_workspace_context(topic="deploy")
            typed(context, "WorkspaceContext", dict)
            check(bool(context), "get_context returned an empty object")

            # ── what cannot be answered locally is refused by name ───────────────
            for label, call in (
                ("briefings", lambda: client.get_latest_briefing()),
                ("revert", lambda: client.manage_memory(id="k_01ARRZ0W6M3CQ8Y5X0Z4T9V1Q2", action="revert")),
            ):
                try:
                    call()
                except KiomonError as error:
                    check(
                        error.code == "unsupported_locally",
                        f"{label} should refuse with `unsupported_locally`, got `{error.code}`",
                    )
                else:
                    FAILED.append(f"{label} answered instead of refusing")

            # ── the door ────────────────────────────────────────────────────────
            # No credential is demanded by default: a local brain has no accounts to check a
            # key against, so the placeholder the SDK sends is ignored and a bare request is
            # answered. What is still refused is the browser, which is the check that does
            # something: a foreign `Origin` and a non-loopback `Host` are both `403`, before
            # routing. (`--require-token` adds `401` for a shared machine.)
            for label, headers, expected in (
                ("no credential at all", {}, 200),
                ("the SDK's placeholder", {"X-API-Key": "local"}, 200),
                ("foreign Origin", {"Origin": "http://evil.example"}, 403),
                ("non-loopback Host", {"Host": "evil.example"}, 403),
            ):
                request = urllib.request.Request(url + "/api/workspaces", headers=headers)
                try:
                    with urllib.request.urlopen(request, timeout=10) as response:
                        check(
                            response.status == expected,
                            f"{label} should be {expected}, got {response.status}",
                        )
                except urllib.error.HTTPError as error:
                    check(
                        error.code == expected,
                        f"{label} should be {expected}, got {error.code}",
                    )
        finally:
            proc.terminate()
            proc.wait(timeout=10)
    finally:
        if not FAILED:
            shutil.rmtree(home, ignore_errors=True)
        else:
            print(f"\nworking directory kept for inspection: {home}", file=sys.stderr)

    if FAILED:
        print(f"\n{len(FAILED)} divergence(s):", file=sys.stderr)
        for line in FAILED:
            print(f"  - {line}", file=sys.stderr)
        return 1
    print("the SDK's declared wire and the local engine agree")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", type=Path, default=Path("target/release/kiomon.exe" if os.name == "nt" else "target/release/kiomon"))
    # The SDKs live beside this project (`MEMORY-INFRASTRUCTURE/SDK`), one level above the
    # `KIOMON` directory this repository sits in — so the default climbs out of both.
    parser.add_argument(
        "--sdk",
        type=Path,
        default=Path(__file__).resolve().parents[3] / "SDK" / "kiomon-py" / "src",
    )
    args = parser.parse_args()
    if not args.bin.exists():
        print(f"no binary at {args.bin}; cargo build --release first", file=sys.stderr)
        return 2
    if not (args.sdk / "kiomon").is_dir():
        print(f"no SDK package under {args.sdk}", file=sys.stderr)
        return 2
    return run(args.bin, args.sdk)


if __name__ == "__main__":
    raise SystemExit(main())
