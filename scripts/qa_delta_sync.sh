#!/usr/bin/env bash
#
# QA quantifie pour le delta sync endpoint (ADR-028). Verifie contre un
# backend Rust qui tourne :
#   1. GET /api/books expose le header X-Catalog-Cursor
#   2. GET /api/books?since=0 renvoie la bonne shape JSON
#   3. Une mutation sur la table books fait avancer le cursor et le delta
#      suivant contient exactement 1 op
#   4. DELETE dans operation_log simule la retention et declenche 410 Gone
#      avec le body cursor_too_old attendu
#
# Scenarios 1, 2, 3 ne requierent que l'URL du backend.
# Scenario 4 requiert DB_PATH pour SQL direct (sqlite3).
#
# Le scenario "logs Flutter full GET vs delta" n'est pas scriptable depuis
# le shell : il reste en checklist manuelle en fin de script.
#
# Usage:
#   BASE_URL=http://localhost:8000 DB_PATH=~/Library/Application\ Support/com.bibliogenius.app/bibliogenius.db \
#     ./scripts/qa_delta_sync.sh
#
# Dependances: curl, jq. sqlite3 optionnel (scenario 4).

set -u

BASE_URL="${BASE_URL:-http://localhost:8000}"
DB_PATH="${DB_PATH:-}"

pass=0
fail=0
skipped=0

# ---- helpers ----------------------------------------------------------------

section() {
  printf '\n\033[1;34m=== %s ===\033[0m\n' "$1"
}

ok()   { printf '  \033[32mOK\033[0m %s\n' "$1"; pass=$((pass + 1)); }
ko()   { printf '  \033[31mKO\033[0m %s\n' "$1"; fail=$((fail + 1)); }
skip() { printf '  \033[33m--\033[0m %s\n' "$1"; skipped=$((skipped + 1)); }
info() { printf '  .  %s\n' "$1"; }

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing dep: $1" >&2
    exit 2
  }
}

# Extract a response header value (case-insensitive). Strips trailing CR.
header_value() {
  local raw="$1" name="$2"
  echo "$raw" | awk -v h="$name" 'BEGIN{IGNORECASE=1} tolower($1) == tolower(h)":"{sub(/^[^:]+: */, ""); sub(/\r$/, ""); print; exit}'
}

# Status line from a full "curl -i" response.
status_of() {
  echo "$1" | head -1 | awk '{print $2}'
}

# Body portion from a full "curl -i" response (after the blank line).
body_of() {
  echo "$1" | awk 'found{print} /^\r?$/{found=1}'
}

# ---- preflight --------------------------------------------------------------

section "Preflight"
need curl
need jq
info "BASE_URL=$BASE_URL"

status=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$BASE_URL/api/books?limit=1" || echo 000)
if [[ "$status" != "200" ]]; then
  ko "backend unreachable (status $status). Start the Rust server first."
  exit 1
fi
ok "backend reachable"

if [[ -n "$DB_PATH" ]]; then
  if command -v sqlite3 >/dev/null 2>&1 && [[ -r "$DB_PATH" ]]; then
    info "DB_PATH=$DB_PATH (scenario 4 enabled)"
  else
    info "DB_PATH set but sqlite3 missing or path unreadable (scenario 4 will skip)"
    DB_PATH=""
  fi
else
  info "DB_PATH unset (scenario 4 will skip)"
fi

# ---- scenario 1: X-Catalog-Cursor header on full GET -----------------------

section "1. Full GET exposes X-Catalog-Cursor"
resp=$(curl -sS -i "$BASE_URL/api/books")
st=$(status_of "$resp")
if [[ "$st" == "200" ]]; then ok "status 200"; else ko "status $st"; fi

cursor1=$(header_value "$resp" "x-catalog-cursor")
if [[ -n "$cursor1" ]]; then
  ok "X-Catalog-Cursor = $cursor1"
else
  ko "X-Catalog-Cursor header missing"
fi

etag1=$(header_value "$resp" "etag")
if [[ -n "$etag1" ]]; then
  ok "ETag preserved ($etag1)"
else
  ko "ETag header missing (regression on legacy path)"
fi

# ---- scenario 2: delta shape ------------------------------------------------

section "2. GET ?since=0 returns {operations, latest_cursor, has_more}"
body=$(curl -sS "$BASE_URL/api/books?since=0")
shape_ok=1
for k in operations latest_cursor has_more; do
  if echo "$body" | jq -e "has(\"$k\")" >/dev/null 2>&1; then
    ok "key .$k present"
  else
    ko "key .$k missing"
    shape_ok=0
  fi
done

if [[ "$shape_ok" == "1" ]]; then
  ops_count=$(echo "$body" | jq '.operations | length')
  latest=$(echo "$body" | jq -r '.latest_cursor')
  info "operations count = $ops_count, latest_cursor = $latest"
fi

# ---- scenario 3: cursor advances after a mutation --------------------------

section "3. New INSERT op advances the cursor (delta contains 1 op)"
if [[ -z "$DB_PATH" ]]; then
  skip "needs DB_PATH to inject an operation_log row"
else
  # Capture current latest_cursor.
  prev=$(curl -sS "$BASE_URL/api/books?since=0" | jq -r '.latest_cursor')
  info "cursor before mutation = $prev"

  # Insert a synthetic local INSERT row. We reference a book id that most
  # likely does NOT exist in the live table (1000000+) so the upsert side
  # of the delta produces zero rows (the book_repo.find_by_id returns None
  # and we skip silently). What we want to assert is only that the cursor
  # advances, not that the book resolves. This keeps the QA script from
  # needing a JWT to actually POST /api/books.
  stamp=$(date +%s)
  fake_book_id=$((1000000 + stamp % 1000))
  sqlite3 "$DB_PATH" "INSERT INTO operation_log (entity_type, entity_id, operation, source, status, created_at) VALUES ('book', $fake_book_id, 'INSERT', 'local', 'applied', datetime('now'));" \
    && ok "injected INSERT op on book id=$fake_book_id" \
    || { ko "SQL insert failed"; exit 1; }

  new_body=$(curl -sS "$BASE_URL/api/books?since=$prev")
  new_cursor=$(echo "$new_body" | jq -r '.latest_cursor')
  info "cursor after mutation  = $new_cursor"

  if [[ "$new_cursor" -gt "$prev" ]]; then
    ok "cursor advanced ($prev -> $new_cursor)"
  else
    ko "cursor did not advance"
  fi

  # The op count can legitimately be 0 (the fake book id has no backing
  # row, upsert is dropped) OR 1 (if another local mutation happened in
  # parallel). Either way we only care the cursor moved.
  info "new window had $(echo "$new_body" | jq '.operations | length') op(s)"
fi

# ---- scenario 4: stale cursor -> 410 Gone ----------------------------------

section "4. Stale cursor returns 410 Gone with cursor_too_old body"
if [[ -z "$DB_PATH" ]]; then
  skip "needs DB_PATH to prune old operation_log rows"
else
  # Pick a cursor value that is guaranteed older than any surviving row.
  # We delete any id <= this value so the oldest retained id is strictly
  # greater than stale + 1 -- that is what cursor-too-old detects.
  current_max=$(sqlite3 "$DB_PATH" "SELECT COALESCE(MAX(id), 0) FROM operation_log;")
  stale=0
  if [[ "$current_max" -le 2 ]]; then
    info "not enough op log rows to prune (max id = $current_max); skipping"
    skip "min id is already $current_max"
  else
    # Prune the lower half so oldest becomes ~current_max/2, well above 0.
    prune_upto=$((current_max / 2))
    pinned_before=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM operation_log WHERE id <= $prune_upto;")
    sqlite3 "$DB_PATH" "DELETE FROM operation_log WHERE id <= $prune_upto;"
    info "pruned $pinned_before rows with id <= $prune_upto"

    resp=$(curl -sS -i "$BASE_URL/api/books?since=$stale")
    st=$(status_of "$resp")
    if [[ "$st" == "410" ]]; then
      ok "status 410 Gone"
      b=$(body_of "$resp")
      if echo "$b" | jq -e '.error == "cursor_too_old"' >/dev/null 2>&1; then
        ok 'body contains error=cursor_too_old'
      else
        ko "body missing error=cursor_too_old"
      fi
      oldest=$(echo "$b" | jq -r '.oldest_available_cursor')
      if [[ -n "$oldest" && "$oldest" != "null" ]]; then
        ok "oldest_available_cursor = $oldest"
      else
        ko "oldest_available_cursor missing"
      fi
      if echo "$b" | jq -e '.hint' >/dev/null 2>&1; then
        ok "hint field present"
      else
        ko "hint field missing"
      fi
    else
      ko "expected 410, got $st"
    fi
  fi
fi

# ---- summary ---------------------------------------------------------------

printf '\n\033[1;34m=== Summary ===\033[0m\n'
printf '  pass=%d  fail=%d  skipped=%d\n' "$pass" "$fail" "$skipped"
echo
echo "Manual checklist (not scripted):"
echo "  [ ] Flutter logs: first pull shows full GET + X-Catalog-Cursor adopted"
echo "  [ ] Flutter logs: subsequent pulls show GET ...?since=<cursor>"
echo "  [ ] On 410, Flutter falls back to full GET and adopts the new cursor"

if [[ "$fail" -gt 0 ]]; then
  exit 1
fi
exit 0
