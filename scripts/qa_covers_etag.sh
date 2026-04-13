#!/usr/bin/env bash
#
# QA quantifié pour le travail "covers + diff-based sync" (session 2026-04-13).
# Vérifie trois scénarios contre un backend Rust qui tourne :
#   1. Cover resize LAN (étape 2)  — GET /api/books/{id}/cover
#   2. If-None-Match 304      (7a) — GET /api/books?owned_only=true
#   3. Hub cover size        (5)   — optionnel, si HUB_URL est défini
#
# Les scénarios E2EE relay (7b, "unchanged") nécessitent deux peers qui
# parlent effectivement via relay : ils restent en checklist manuelle en
# fin de script.
#
# Usage:
#   BASE_URL=http://192.168.1.53:8000 ./qa_covers_etag.sh
#
# Dépendances: curl, jq, file, awk. Aucun setup côté serveur requis.

set -u

BASE_URL="${BASE_URL:-http://localhost:8000}"
MAX_COVER_BYTES=51200    # 50 KB — soft cap promis
MAX_COVER_WIDTH=300
MAX_COVER_HEIGHT=450

pass=0
fail=0

# ---- helpers ----------------------------------------------------------------

section() {
  printf '\n\033[1;34m=== %s ===\033[0m\n' "$1"
}

ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; pass=$((pass + 1)); }
ko()   { printf '  \033[31m✗\033[0m %s\n' "$1"; fail=$((fail + 1)); }
info() { printf '  · %s\n' "$1"; }

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing dep: $1" >&2
    exit 2
  }
}

# ---- preflight --------------------------------------------------------------

section "Preflight"
need curl
need jq
need file
need awk

info "BASE_URL=$BASE_URL"

status=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$BASE_URL/api/books?limit=1" || echo 000)
if [[ "$status" != "200" ]]; then
  ko "backend unreachable (status $status). Start the Rust server first."
  exit 1
fi
ok "backend reachable"

# Pick a book that actually has a LOCAL cover. External HTTP URLs
# (Google Books, OpenLibrary) are intentionally rejected by
# get_book_cover — it only serves covers stored on disk.
book_id=$(
  curl -s "$BASE_URL/api/books?owned_only=true" |
    jq -r '
      .books[]
      | select(.cover_url != null)
      | select(.cover_url | startswith("http") | not)
      | .id
    ' |
    head -1
)
if [[ -z "${book_id:-}" || "$book_id" == "null" ]]; then
  ko "no book with a LOCAL cover_url found"
  info "external http(s) covers are not served by get_book_cover by design"
  info "upload a custom cover from the app and retry"
  exit 1
fi
ok "using book id=$book_id for cover checks"

# ---- scenario 1 : LAN cover resize -----------------------------------------

section "1. LAN cover resize (étape 2)"

cover_file=$(mktemp -t qa_cover.XXXXXX)
trap 'rm -f "$cover_file"' EXIT

headers=$(curl -s -D - "$BASE_URL/api/books/$book_id/cover" -o "$cover_file")
status=$(printf '%s' "$headers" | awk 'NR==1 {print $2}')
content_type=$(printf '%s' "$headers" | awk -F': ' 'tolower($1)=="content-type" {print $2}' | tr -d '\r')
cache_control=$(printf '%s' "$headers" | awk -F': ' 'tolower($1)=="cache-control" {print $2}' | tr -d '\r')

if [[ "$status" == "200" ]]; then ok "HTTP 200"; else ko "HTTP $status (expected 200)"; fi

if [[ "$content_type" == "image/jpeg" ]]; then
  ok "Content-Type is image/jpeg"
else
  ko "Content-Type is '$content_type' (expected image/jpeg)"
fi

if [[ "$cache_control" == *"max-age"* && "$cache_control" == *"must-revalidate"* ]]; then
  ok "Cache-Control has max-age + must-revalidate"
else
  ko "Cache-Control='$cache_control' missing max-age or must-revalidate"
fi

size=$(wc -c < "$cover_file" | awk '{print $1}')
info "cover size = $size bytes"
if (( size <= MAX_COVER_BYTES )); then
  ok "size ≤ ${MAX_COVER_BYTES} B soft cap"
else
  ko "size $size > ${MAX_COVER_BYTES} B (soft cap exceeded)"
fi

dims=$(file -b "$cover_file" | awk -F 'precision [0-9]+,' '{print $NF}')
width=$(printf '%s' "$dims"  | grep -oE '[0-9]+' | awk 'NR==1')
height=$(printf '%s' "$dims" | grep -oE '[0-9]+' | awk 'NR==2')
info "dimensions = ${width:-?}x${height:-?}"
if [[ -n "${width:-}" && -n "${height:-}" ]] \
   && (( width  <= MAX_COVER_WIDTH ))  \
   && (( height <= MAX_COVER_HEIGHT )); then
  ok "dimensions ≤ ${MAX_COVER_WIDTH}x${MAX_COVER_HEIGHT}"
else
  ko "dimensions exceed ${MAX_COVER_WIDTH}x${MAX_COVER_HEIGHT}"
fi

# ---- scenario 2 : If-None-Match 304 ----------------------------------------

section "2. If-None-Match 304 on /api/books (étape 7a)"

first=$(mktemp -t qa_books_first.XXXXXX)
trap 'rm -f "$cover_file" "$first"' EXIT

first_headers=$(curl -s -D - "$BASE_URL/api/books?owned_only=true" -o "$first")
first_status=$(printf '%s' "$first_headers" | awk 'NR==1 {print $2}')
first_etag=$(printf '%s' "$first_headers" | awk -F': ' 'tolower($1)=="etag" {print $2}' | tr -d '\r')
first_size=$(wc -c < "$first" | awk '{print $1}')

if [[ "$first_status" == "200" ]]; then ok "first GET → 200"; else ko "first GET → $first_status"; fi
if [[ -n "$first_etag" ]]; then
  ok "ETag present: $first_etag"
else
  ko "ETag missing on first response"
fi
info "first body size = $first_size bytes"

second_headers=$(
  curl -s -D - \
    -H "If-None-Match: $first_etag" \
    -o /dev/null \
    "$BASE_URL/api/books?owned_only=true"
)
second_status=$(printf '%s' "$second_headers" | awk 'NR==1 {print $2}')
second_etag=$(printf '%s' "$second_headers" | awk -F': ' 'tolower($1)=="etag" {print $2}' | tr -d '\r')
second_cl=$(printf '%s' "$second_headers" | awk -F': ' 'tolower($1)=="content-length" {print $2}' | tr -d '\r')

if [[ "$second_status" == "304" ]]; then
  ok "second GET (If-None-Match) → 304 Not Modified"
else
  ko "second GET → $second_status (expected 304)"
fi
if [[ -n "$first_etag" && "$second_etag" == "$first_etag" ]]; then
  ok "304 re-emits the same ETag (RFC 7232 §4.1)"
elif [[ -z "$first_etag" ]]; then
  ko "cannot verify 304 ETag echo: first ETag was missing"
else
  ko "ETag mismatch on 304: '$second_etag' vs '$first_etag'"
fi
if [[ -z "$second_cl" || "$second_cl" == "0" ]]; then
  ok "304 body is empty"
else
  ko "304 carries Content-Length=$second_cl (should be empty)"
fi

if (( first_size > 0 )); then
  # Bytes économisés sur un refresh sans changement : tout le body.
  saving=$(awk "BEGIN { printf \"%.1f\", ($first_size / 1024) }")
  info "savings per no-op refresh ≈ ${saving} KB"
fi

# Sanity check : ETag stable si on refait un premier GET sans If-None-Match.
third_headers=$(curl -s -D - -o /dev/null "$BASE_URL/api/books?owned_only=true")
third_etag=$(printf '%s' "$third_headers" | awk -F': ' 'tolower($1)=="etag" {print $2}' | tr -d '\r')
if [[ -n "$first_etag" && "$third_etag" == "$first_etag" ]]; then
  ok "ETag deterministic across repeated GETs"
elif [[ -z "$first_etag" ]]; then
  ko "cannot verify determinism: no ETag emitted — backend likely needs rebuild"
else
  ko "ETag changed without catalog mutation: '$third_etag' vs '$first_etag'"
fi

# ---- scenario 3 : hub cover size (optional) --------------------------------

section "3. Hub cover size (étape 5) — optional"

if [[ -z "${HUB_URL:-}" ]]; then
  info "HUB_URL not set — skipping"
else
  node_id=$(
    curl -s "$BASE_URL/api/directory/config" 2>/dev/null |
      jq -r '.node_id // empty'
  )
  if [[ -z "$node_id" ]]; then
    info "node_id unreachable (peer not registered in directory?) — skipping"
  else
    hub_url="${HUB_URL%/}/api/directory/$node_id/covers/$book_id"
    info "GET $hub_url"
    hub_file=$(mktemp -t qa_hub.XXXXXX)
    trap 'rm -f "$cover_file" "$first" "$hub_file"' EXIT

    hub_headers=$(curl -sL -D - "$hub_url" -o "$hub_file")
    hub_status=$(printf '%s' "$hub_headers" | awk 'NR==1 {print $2}')
    hub_size=$(wc -c < "$hub_file" | awk '{print $1}')

    if [[ "$hub_status" == "200" ]]; then
      ok "hub cover served (HTTP 200)"
      info "hub cover size = $hub_size bytes"
      if (( hub_size <= MAX_COVER_BYTES )); then
        ok "hub cover ≤ ${MAX_COVER_BYTES} B soft cap"
      else
        ko "hub cover $hub_size > ${MAX_COVER_BYTES} B"
      fi
    else
      info "hub returned $hub_status — peer probably hasn't synced this cover yet"
    fi
  fi
fi

# ---- manual checklist -------------------------------------------------------

section "Manual checks (not automated)"
cat <<'EOF'
  □ Scenario 6 (E2EE "unchanged" via relay) needs two real peers through
    the hub. Tail the Rust logs on the responder and look for
      "book_sync: status=unchanged"
    after a no-op refresh from the requester (5G mode).

  □ Scenario 7 (add one book, observe diff sync) is a manual smoke test:
    capture /api/books payload size before + after adding a book.

  □ Visual quality: open a cover on the detail screen (iPhone Pro ×3).
    Text on the cover should be readable (no blocky blur). If pixelated,
    consider raising dims to 400x600 in utils/cover_image.rs.
EOF

# ---- summary ----------------------------------------------------------------

section "Summary"
printf '  %s passed · %s failed\n' "$pass" "$fail"
if (( fail > 0 )); then
  exit 1
fi
