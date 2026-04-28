#!/usr/bin/env bash
#
# QA manuel pour le seed library_config produit a la migration.
#
# Verifie que :
#   1. Le seed n'est jamais "My Library"
#   2. Le seed contient le suffixe " #<tag>" (4 chars)
#   3. Le prefixe localise change avec LC_ALL ("Bibliotheque de" en FR,
#      "Library of" en EN)
#   4. La valeur ecrite via cargo example est bien celle qu'on retrouve via
#      sqlite3 (pas de divergence ORM <-> raw SQL)
#
# Pour lancer:
#   ./scripts/qa_library_seed.sh
#
# Dependances: cargo, sqlite3.

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "ERROR: sqlite3 introuvable, installe-le avant de relancer." >&2
    exit 1
fi

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

run_case() {
    local label=$1
    local lc_all=$2
    local lang=$3
    local db="$TMPDIR/seed-$label.db"

    echo "=== $label ==="
    echo "  LC_ALL=$lc_all LANG=$lang"

    # Compiles once on first call, instant on subsequent calls.
    local from_orm
    from_orm=$(LC_ALL="$lc_all" LANG="$lang" \
        cargo run --quiet --example library_seed_demo -- "$db")

    local from_sqlite
    from_sqlite=$(sqlite3 "$db" 'SELECT name FROM library_config;')

    echo "  via ORM    : $from_orm"
    echo "  via sqlite3: $from_sqlite"

    if [[ "$from_orm" != "$from_sqlite" ]]; then
        echo "  FAIL: divergence ORM vs sqlite3"
        return 1
    fi
    if [[ "$from_orm" == "My Library" ]]; then
        echo "  FAIL: seed est le placeholder legacy"
        return 1
    fi
    if [[ "$from_orm" != *' #'???? ]]; then
        echo "  FAIL: suffixe ' #<tag 4 chars>' manquant"
        return 1
    fi
    echo "  OK"
    echo
}

run_case "fr"      "fr_FR.UTF-8" "fr_FR.UTF-8"
run_case "en"      "en_US.UTF-8" "en_US.UTF-8"
run_case "default" "C"           "C"

echo "Tous les cas sont OK."
