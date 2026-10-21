#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The whole lifecycle in one script, against a throwaway brain.
#
#   bash examples/cli-tour.sh
#
# Nothing here needs a running server, a network, or an account. Every command prints
# what it is about to run, so the script doubles as the annotated walkthrough. Stop
# reading and run it  -  the output is the documentation.
#
# The brain lives in a temporary directory and is deleted on exit. Set KEEP=1 to keep it
# and print its path, which is how you poke at the same data afterwards.

set -eu

# Find the binary: $KIOMON_BIN, then the release build in this repository, then $PATH.
# A script that silently ran a different `kiomon` than the one you just built would make
# every example here suspect, so the resolved path is printed first.
KIOMON_BIN="${KIOMON_BIN:-}"
if [ -z "$KIOMON_BIN" ]; then
  if [ -x "target/release/kiomon" ]; then
    KIOMON_BIN="target/release/kiomon"
  elif [ -x "target/release/kiomon.exe" ]; then
    KIOMON_BIN="target/release/kiomon.exe"
  else
    KIOMON_BIN="kiomon"
  fi
fi

WORK="$(mktemp -d)"
export KIOMON_DB="$WORK/brain.db"

cleanup() {
  if [ "${KEEP:-0}" = "1" ]; then
    printf '\nKept the brain at %s\n' "$KIOMON_DB" >&2
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT INT TERM

# Run a command with its arguments shown. `set -x` would be one character shorter and
# would print shell-quoting noise instead of the command a reader would type.
run() {
  printf '\n\033[1m$ kiomon %s\033[0m\n' "$*"
  "$KIOMON_BIN" "$@"
}

step() {
  printf '\n\033[36m-- %s ---------------------------------------------\033[0m\n' "$1"
}

printf 'kiomon: %s\n' "$KIOMON_BIN"
"$KIOMON_BIN" --version

# -- 1. Setup ----------------------------------------------------------------
# `init` is idempotent: it creates the file and the `default` workspace, and running it
# again never resets a workspace's review mode. `doctor` is the "where am I" command  - 
# it prints the resolved database path, the schema version, the embedder, and the decay
# backlog, and it is what you run first when something looks empty.
step "1. Setup: one file, one workspace"
run init
run doctor
run stats

# -- 2. Write ----------------------------------------------------------------
# A kind is required because it drives decay (references never decay, routines outlive
# episodes) and because it is a facet of every search. `--tag`, `--entities`, `--claims`
# and `--domain` are the retrieval surfaces a model actually reads.
step "2. Write: two memories"
run add \
  --kind semantic \
  --title "Staging deploys need a manual migration" \
  --body "Run migrations before promoting the worker. A promote without migrations leaves the worker reading a schema it does not know." \
  --excerpt "Migration first, then promote." \
  --tag deploy --tag staging \
  --entities "worker,staging" \
  --claims "migrations must precede a promote" \
  --domain infra \
  --confidence 0.9

run add \
  --kind procedural \
  --title "Deploy checklist" \
  --body "1. Run migrations. 2. Promote the worker. 3. Smoke test /health. 4. Watch the error rate for five minutes." \
  --tag deploy

# Note the line each `add` prints: a write also ages the brain while it is open (a 100-row
# page of `decay`), because a nudge attached to writes is the whole scheduler this engine
# has. `kiomon decay --dry-run` says what it would do without doing it.
#
# A user's own save activates immediately. An agent's *reflection* has to earn it:
# at or above 0.75 it activates, below that it waits in the review inbox. This is the
# write gate, and it is the same rule the hosted product uses.
step "3. A write that waits for review"
run add \
  --kind semantic \
  --source agent_reflection \
  --confidence 0.4 \
  --title "The worker retries twice" \
  --body "Unverified: the worker appears to retry a failed job twice before dead-lettering it."
run review list

# -- 3. Read -----------------------------------------------------------------
# `search` is for finding one thing; `retrieve` is for deciding what to do. Search ranks
# by relevance; retrieve ranks by what the intent needs and returns at most one procedure,
# because two procedures in a packet is an agent improvising between them.
step "4. Search: what matches the words"
run search deploy

step "5. Retrieve: what to do about it"
run retrieve --intent execute "how do I deploy to staging?" --no-record
run retrieve --intent plan "what needs to happen before a promote?"

# -- 4. Graph ----------------------------------------------------------------
# `add` auto-linked the checklist to its neighbours when it was written. Explicit edges
# are how you say something the words do not: this memory supersedes that one, this one
# contradicts that one.
step "6. Edges: what the words cannot say"
FACT=$("$KIOMON_BIN" search "manual migration" --no-record --json \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['results'][0]['id'])")
CHECK=$("$KIOMON_BIN" search "deploy checklist" --no-record --json \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['results'][0]['id'])")
run relate "$FACT"
run link "$CHECK" "$FACT" --relation part_of
run get "$FACT" --relations

# -- 5. Lifecycle ------------------------------------------------------------
# Rating says "this helped" (+1) or "it did not" (-1). Outcome says what happened to the
# *task* and how each memory performed in it  -  the signal that lets a brain re-rank
# honestly instead of by recency.
step "7. Feedback: earn the rank"
run rate "$FACT" +1
run outcome success --item "$FACT:helpful" --item "$CHECK:helpful"

# Pin exempts a memory from decay. Everything else ages: strength halves on a per-kind
# half-life (30 days for episodes, 90 for procedures, 180 for facts), stretched by how
# often it was retrieved, and archives below 0.05.
step "8. Decay and the pin that stops it"
run pin "$FACT"
run decay --dry-run

# -- 6. Review and portability ------------------------------------------------
# Approving a draft indexes it, links it, and makes it retrievable. Rejecting archives it
# without ever indexing  -  the difference matters, because a rejected draft must not come
# back in next week's search.
step "9. The review inbox, resolved"
PENDING=$("$KIOMON_BIN" review list --json \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['data']['items'][0]['id'])")
run review approve "$PENDING"
run review list

# Export is one JSON object per line  -  a memory is a line, not a rendering. Import is a
# commit, not a parse: it reports per line and keeps what was valid.
step "10. Portability: a file you own"
run export --format jsonl --output "$WORK/export.jsonl"
printf '\n\033[1m$ head -c 220 %s\033[0m\n' "$WORK/export.jsonl"
head -c 220 "$WORK/export.jsonl"; printf '\n'
run import "$WORK/export.jsonl" --dry-run

# Forgetting is a hard delete, and it takes the edges with it. Nothing here is soft.
step "11. Forget"
run forget "$CHECK" --yes
run stats

printf '\n\033[32mDone.\033[0m Read examples/README.md for the SDK and MCP doors into this same brain.\n'
