#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The HTTP wire, with no SDK and nothing to install but `curl`.
#
#   bash examples/curl.sh
#
# This is the door the published SDKs call. Reading it once is how you debug them: every
# SDK method is a method, a path and a JSON body, and the three refusals (Host, Origin,
# token) happen before the router looks at the path at all.
#
# The script starts its own `serve` on an ephemeral port with a throwaway brain, reads the
# token out of the file `serve` created, and kills the server on exit.

set -eu

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
SERVER_PID=""

cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

step() { printf '\n\033[36m-- %s ---------------------------------------------\033[0m\n' "$1"; }

# -- Start the server --------------------------------------------------------
# `--port 0` asks the OS for a free port; the first line of stdout is one JSON object
# describing what is listening. That line is the interface a supervisor parses, and it is
# printed before the first request can arrive, so this read cannot race.
step "Starting serve on an ephemeral port"
"$KIOMON_BIN" init >/dev/null
"$KIOMON_BIN" add --quiet --kind semantic \
  --title "Staging deploys need a manual migration" \
  --body "Run migrations before promoting the worker." \
  --tag deploy >/dev/null

"$KIOMON_BIN" serve --port 0 --json > "$WORK/serve.json" 2>"$WORK/serve.err" &
SERVER_PID=$!
# Wait for the announcement line rather than sleeping a guessed interval.
for _ in $(seq 1 100); do
  [ -s "$WORK/serve.json" ] && break
  sleep 0.05
done
# Read the announcement through stdin, not by path: on Git Bash `$WORK` is a POSIX path
# that bash translates and a Windows Python does not, so a `json.load(open(...))` here
# would fail on the one platform this repository is developed on.
URL=$(python3 -c "import json,sys;print(json.load(sys.stdin)['url'])" < "$WORK/serve.json")
TOKEN_FILE=$(python3 -c "import json,sys;print(json.load(sys.stdin)['token_file'])" < "$WORK/serve.json")
TOKEN=$(cat "$TOKEN_FILE")
printf 'listening: %s\ntoken file: %s\n' "$URL" "$TOKEN_FILE"

# Bearer and X-API-Key are the same credential. The SDKs send X-API-Key; a shell script
# reads better with a header it can name.
API=(-H "X-API-Key: $TOKEN")

# `call` prints the request line, the response body, and the status  -  the three things you
# need when a call that "should work" does not. It also leaves the body in `$LAST` (the
# status is the last line `curl -w` appends), which is how the script picks an id out of a
# response without a second request.
LAST=""
call() {
  local method="$1" path="$2" body="${3:-}" raw status
  printf '
[1m%s %s[0m
' "$method" "$path"
  if [ -n "$body" ]; then
    raw=$(curl -sS -X "$method" "${API[@]}" -H 'Content-Type: application/json'       -d "$body" -w '
%{http_code}' "$URL$path")
  else
    raw=$(curl -sS -X "$method" "${API[@]}" -w '
%{http_code}' "$URL$path")
  fi
  status=${raw##*$'
'}
  LAST=${raw%$'
'*}
  printf '%s
[%s]
' "$LAST" "$status"
}

# -- Read --------------------------------------------------------------------
# /health is the only route with no database behind it: it answers before the file is
# opened, which is what makes it the right readiness probe.
step "Read: health, search, retrieve"
call GET "/health"
call GET "/api/workspaces"
call GET "/api/search?q=deploy&limit=3"

# The packet, not the search results: this is the call an agent should make before acting.
# `execute` asks for the one procedure that fits; the answer carries provenance and
# warnings a search result cannot.
call POST "/api/retrieve" '{"intent":"execute","query":"how do I deploy to staging?"}'

# -- Write -------------------------------------------------------------------
# The SDK's `draftMemory` is this route. A high-confidence reflection activates; below the
# threshold it lands in the review inbox, which is why the answer has both `promoted` and
# `pending`  -  check them, do not assume the write is live.
step "Write: a draft through the write gate"
call POST "/api/memories/batch-draft" '{
  "workspace_id": "default",
  "learnings": [
    {"kind":"procedural","title":"Roll back a bad deploy",
     "body":"Promote the previous tag, then run the migrations that tag expects.",
     "tags":["deploy"],"confidence":0.9}
  ]
}'

# `promoted` holds what activated; `pending` holds what the review inbox took. Reading the
# id out of the write's own answer is the honest way to continue: a search on the next line
# could legitimately miss, and then the script would be asserting the ranking instead of the
# write.
ID=$(printf '%s' "$LAST"   | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['promoted'][0] if d['promoted'] else d['pending'][0])")

# -- One document, and its lifecycle -----------------------------------------
step "One document: read, pin, rate"
call GET "/api/documents/$ID"
call PATCH "/api/documents/$ID" '{"pinned":1}'
call POST "/api/documents/$ID/rate" '{"rating":1}'

# -- The refusals ------------------------------------------------------------
# These are not decorations. Without the Host check an attacker's domain pointed at
# 127.0.0.1 arrives with its own Host and reads your brain (DNS rebinding); without the
# token check any local process can. Both refusals happen before routing, so an
# unauthorised client cannot map the surface by comparing 401 with 404.
step "The three refusals"
printf '\n\033[1mno token\033[0m\n'
curl -sS "$URL/api/workspaces" -w '\n[%{http_code}]\n'
printf '\n\033[1mforeign Host\033[0m\n'
curl -sS "${API[@]}" -H 'Host: attacker.example' "$URL/api/workspaces" -w '\n[%{http_code}]\n'
printf '\n\033[1mforeign Origin\033[0m\n'
curl -sS "${API[@]}" -H 'Origin: https://attacker.example' "$URL/api/workspaces" -w '\n[%{http_code}]\n'

# -- Honest gaps -------------------------------------------------------------
# A route this engine cannot answer says so with a code, never an empty 200: a client
# cannot tell "nothing there" from "not implemented", and one of those makes it stop
# asking. `revert` needs revision history; a briefing is composed by a model.
step "A route this engine cannot answer"
call POST "/api/memories/$ID/revert"

printf '\n\033[32mDone.\033[0m The same verbs are in examples/cli-tour.sh, over a file instead of a socket.\n'
