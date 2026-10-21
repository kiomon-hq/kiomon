#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The loop an agent should actually run: retrieve, act, report, learn.

    python examples/python/agent_loop.py

Everything is the same local brain the quickstart used. The point of this file is the shape of
the integration rather than the API surface: a packet goes into a prompt, the answer's
provenance comes back as feedback, and a write the agent was not sure about waits for a human
instead of quietly becoming a fact.
"""

from __future__ import annotations

import local

KiomonError = local.KiomonError


def section(title: str) -> None:
    print(f"\n-- {title} " + "-" * max(0, 62 - len(title)))


def packet_as_prompt(packet: dict) -> str:
    """Render a packet the way you would put it in a system prompt.

    The packet's fields are already chosen for this: `claim` is the actionable line (a fact, a
    routine's steps, or an episode's outcome), `applicability` says whether it fits the
    constraints you passed, and `warnings` is not decoration - a low-confidence memory is
    exactly the one a model should hedge about.
    """
    lines = [f"Context ({packet['summary']})"]
    for fact in packet["facts"]:
        lines.append(f"- [{fact['confidence']:.2f}] {fact['claim']}")
    procedure = packet.get("procedure")
    if procedure:
        lines.append(f"- run: {procedure['claim']}")
    for episode in packet["episodes"]:
        lines.append(f"- previously: {episode['claim']}")
    if packet["warnings"]:
        lines.append("Caveats: " + "; ".join(packet["warnings"]))
    return "\n".join(lines)


def main() -> None:
    with local.brain() as (client, url, _database):
        print(f"serving {url}")

        # -- Seed the brain ----------------------------------------------------
        # Two writes at different confidence, because the gate is the thing most
        # integrations get wrong: they assume a successful draft is a live memory.
        section("Seeding: one write that activates, one that waits")
        client.draft_memory(
            {
                "kind": "procedural",
                "title": "Roll back a bad deploy",
                "body": (
                    "Promote the previous tag, then run the migrations that tag expects. "
                    "Never roll back code without rolling back the schema."
                ),
                "tags": ["deploy", "rollback"],
                "confidence": 0.9,
            }
        )
        uncertain = client.draft_memory(
            {
                "kind": "semantic",
                "title": "The worker retries twice",
                "body": "Unverified: a failed job appears to retry twice before dead-lettering.",
                "tags": ["worker", "retries"],
                "confidence": 0.4,  # below the 0.75 gate: this cannot activate itself
            }
        )
        print(f"  pending   {uncertain['pending']}")

        # -- Ask, retrieve, act ------------------------------------------------
        question = "how do I roll back a bad deploy?"
        section(f"Ask: {question}")
        packet = client.retrieve_memory(intent="execute", query=question)
        # This is the integration: the rendered context goes into the prompt, and the ids go
        # into a variable for the feedback step. Never report an id you were not given.
        print(packet_as_prompt(packet))
        answer = "Promote the previous tag first, then run that tag's migrations."  # "the model"
        print(f"  answer    {answer}")

        # -- Report what helped ------------------------------------------------
        # `unused` is a real verdict, not a no-op: it credits a zero, which is how a memory
        # that was retrieved and ignored stops outranking one that was used. Report every id
        # you were handed, not just the ones you liked.
        section("Report: which of those actually helped")
        assessments = [
            {"memory_id": procedure_id, "assessment": "helpful"}
            for procedure_id in [
                item["memory_id"]
                for item in [packet["procedure"]] if item is not None
            ]
        ]
        assessments += [
            {"memory_id": fact["memory_id"], "assessment": "unused"}
            for fact in packet["facts"]
        ]
        reported = client.record_outcome(outcome="success", items=assessments)
        for row in reported["updated"]:
            print(
                f"  {row['id']}  {row['assessment']:<9} "
                f"confidence {row['confidence']:.2f}  feedback {row['feedback_score']:+.2f}"
            )

        # -- The gate, resolved by a human -------------------------------------
        # A draft the agent was unsure about is not in any packet - it is in the inbox, where
        # `kiomon review list` shows it. Approving is what puts it in front of the model.
        section("The write that had to wait")
        before = client.search_memory(query="retries twice")
        print(f"  before    {len(before['results'])} hit(s) - the draft is not indexed")
        draft_id = uncertain["pending"][0]
        client.manage_memory(id=draft_id, action="approve")
        after = client.search_memory(query="retries twice")
        print(f"  approved  {draft_id}")
        print(f"  after     {len(after['results'])} hit(s), status {client.get_memory(draft_id)['status']}")

        # -- Why any of this matters -------------------------------------------
        # The next packet sees the outcome: retrieval_count moved, confidence moved, and the
        # memory that was used ranks ahead of the one that was merely retrieved.
        section("The next task sees what the last one taught")
        again = client.retrieve_memory(intent="execute", query=question)
        print(packet_as_prompt(again))
        used = client.get_memory(packet["procedure"]["memory_id"])
        print(
            f"  procedure retrieved {used['retrieval_count']}x, "
            f"confidence {used['confidence']:.2f}, strength {used['strength']:.2f}"
        )

        # -- Failure is a first-class path -------------------------------------
        # A packet that matched nothing is a packet with an empty summary, not an exception.
        # Raising instead would make an agent's "I do not know" branch impossible.
        section("Asking about something nobody wrote down")
        empty = client.retrieve_memory(intent="answer", query="what is our pager rotation?")
        print(f"  summary   {empty['summary']}")
        print(f"  ids       {empty['retrieved_ids']}")
        try:
            client.get_memory("k_00000000000000000000000000")
        except KiomonError as error:
            print(f"  unknown   {error.status} {error.code}")

    print("\nDone. That loop - retrieve, act, report - is the whole of it.")


if __name__ == "__main__":
    main()
