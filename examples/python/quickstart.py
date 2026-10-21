#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The Python SDK against a local brain: save, search, retrieve, decide, report.

    python examples/python/quickstart.py

Run it with nothing else running. It starts `kiomon serve` on an ephemeral port with a
throwaway database, drives the real SDK against it, and stops it on exit. Set KIOMON_URL and
KIOMON_TOKEN to use a server that is already running instead (see `local.py`).
"""

from __future__ import annotations

import local

Kiomon, KiomonError = local.Kiomon, local.KiomonError


def section(title: str) -> None:
    print(f"\n-- {title} " + "-" * max(0, 62 - len(title)))


def main() -> None:
    with local.brain() as (client, url, database):
        section("1. The connection")
        # Every call below is an HTTP request to the URL printed above. The SDK's defaults --
        # retries, timeouts, error mapping -- all still apply; only the host changed.
        print(f"  base_url  {url}")
        workspaces = client.list_workspaces()["workspaces"]
        print(f"  workspace {workspaces[0]['id']} ({workspaces[0]['name']}), slug={workspaces[0]['slug']!r}")
        # Resolving the workspace is a real step: with one workspace the SDK picks it, with
        # several it refuses to guess. Here we passed workspace_id explicitly, which skips it.
        client.select_workspace(workspaces[0]["id"])

        section("2. Save: an agent reflection")
        # `draft_memory` is sugar over `reflect_session` -- one learning, one write, through
        # the write gate. At confidence >= 0.75 with an autonomous workspace it activates;
        # below that it waits in the review inbox. The answer says which, so read it.
        drafted = client.draft_memory(
            {
                "kind": "procedural",
                "title": "Roll back a bad deploy",
                "body": (
                    "Promote the previous tag, then run the migrations that tag expects. "
                    "Never roll back the code without rolling back the schema."
                ),
                "excerpt": "Previous tag first, migrations second.",
                "tags": ["deploy", "rollback"],
                "confidence": 0.9,
                "entities": ["worker", "migrations"],
                "claims": ["a rollback must match its migrations"],
                "domain": "infra",
            }
        )
        print(f"  drafted   {drafted['drafted']}")
        print(f"  promoted  {drafted['promoted']}")
        print(f"  pending   {drafted['pending']}")
        rollback_id = (drafted["promoted"] or drafted["pending"])[0]

        section("3. Save: a user's own fact")
        # A user save is a different door with the same name in the SDK -- the CLI's `add`.
        # It carries higher authority, so it activates without review. There is no separate
        # "I meant it" flag: the source is the flag.
        fact = client.draft_memory(
            {
                "kind": "semantic",
                "title": "Staging deploys need a manual migration",
                "body": "Run migrations before promoting the worker to staging.",
                "excerpt": "Migration first, then promote.",
                "tags": ["deploy", "staging"],
                "confidence": 0.95,
            }
        )
        fact_id = (fact["promoted"] or fact["pending"])[0]

        section("4. Query: search")
        # `search_memory` is the "find me something" call. Results carry `snippet` (the
        # highlighted match), `rank` (the fused score) and `tags` as a JSON string --
        # `json.loads` it, which is what the hosted wire does too.
        found = client.search_memory(query="deploy", limit=10)
        for hit in found["results"]:
            import json

            tags = json.loads(hit["tags"]) if hit["tags"] else []
            print(f"  {hit['id']}  {hit['kind']:<10} {hit['title']}  {tags}")
        print(f"  legs      {found['legs']}  (vector is false: no embedder configured)")

        section("5. Query: retrieve -- the packet")
        # `retrieve_memory` is the call to make before acting. It returns facts, at most one
        # procedure, episodes, evidence and warnings, ranked by what you are trying to do.
        # `retrieved_ids` is what you report back in step 7.
        packet = client.retrieve_memory(
            intent="execute", query="how do I roll back a bad deploy?"
        )
        print("  intent    execute")
        print(f"  summary   {packet['summary']}")
        for item in packet["facts"]:
            print(f"  fact      {item['memory_id']}  conf {item['confidence']:.2f}  {item['claim']}")
        # `procedure` is singular on purpose: two procedures in one packet is an agent
        # improvising between them, so the packet picks the best one and says how sure it is.
        procedure = packet.get("procedure")
        if procedure:
            print(f"  procedure {procedure['memory_id']}  {procedure['claim']}")
        print(f"  warnings  {packet['warnings'] or 'none'}")
        print(f"  ids       {packet['retrieved_ids']}")

        section("6. Read one memory")
        document = client.get_memory(rollback_id)
        print(f"  {document['title']}  [{document['kind']}, {document['status']}]")
        print(f"  body      {document['body'][:70]}...")
        print(f"  tags      {document['tags']!r}  (a JSON string on the wire, by design)")

        section("7. Feedback: report what actually helped")
        # This is the half of the loop most integrations skip, and skipping it is why a
        # memory org rots: without an outcome, nothing can re-rank honestly later.
        outcome = client.record_outcome(
            outcome="success",
            items=[
                {"memory_id": rollback_id, "assessment": "helpful"},
                {"memory_id": fact_id, "assessment": "helpful"},
            ],
        )
        print(f"  outcome   success  updated={outcome.get('updated')}")

        # A rating is the coarser signal: +1 or -1, no task attached.
        client.manage_memory(id=rollback_id, action="rate", value=1)

        # Read the memory back: feedback_count and feedback_score moved, and so did
        # formula-owned `confidence`. `strength` did not -- decay owns that.
        after = client.get_memory(rollback_id)
        print(
            f"  after     feedback {after['feedback_score']:+.2f} over "
            f"{after['feedback_count']} rating(s), confidence {after['confidence']:.2f}, "
            f"strength {after['strength']:.2f}"
        )

        section("8. Lifecycle and the graph")
        # Pinning is the exception to decay: a pinned memory never ages out.
        client.manage_memory(id=rollback_id, action="pin")
        neighbours = client.explore_memory_graph(id=rollback_id)
        print(f"  nodes     {neighbours['totalNodes']}  edges {len(neighbours['edges'])}")
        # Deeper walks are a v1.1 feature, and the server says so with a code rather than
        # an empty graph -- which is the difference between "no neighbours" and "not yet".
        try:
            client.explore_memory_graph(id=rollback_id, depth=2)
        except KiomonError as error:
            print(f"  depth 2   refused: {error.status} {error.code}")

        section("9. The same brain from a shell")
        # The server and the CLI are the same file and the same functions. This is the
        # property that makes a local brain debuggable: nothing the SDK did is invisible to
        # `kiomon list`.
        if str(database) != "<remote>":
            print(f"  kiomon --db {database} list")
            print(f"  kiomon --db {database} search deploy")
            print("  (re-run with KIOMON_EXAMPLE_DB=./my-brain.db to keep it and try the above)")

    print("\nDone. agent_loop.py is the same brain used the way an agent should use it.")


if __name__ == "__main__":
    main()
