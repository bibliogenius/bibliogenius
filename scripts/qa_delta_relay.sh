#!/usr/bin/env bash
#
# QA companion pour ADR-029 (delta sync via E2EE relay).
#
# Contrairement a ADR-028 (full-LAN, localhost), les scenarios relay se
# jouent a DEUX appareils : iPhone + Mac. Le script tourne sur MAC (qui a
# shell + DB) et automatise ce qui est automatisable ; les verifications
# utilisateur cote iPhone restent manuelles (logs + UX). Chaque commande
# affiche les lignes a guetter cote iPhone.
#
# Pre-requis :
#   - Mac et iPhone tous les deux sur les commits courants (post-ADR-029)
#   - Appariement E2EE fait, au moins une sync reussie
#   - iPhone sur PeerBookListScreen de la lib du Mac, 5G ou WiFi sans le
#     Mac (pour forcer le chemin relay)
#   - Flutter run du Mac visible (pour tracer les WS nudges envoyes)
#
# Usage :
#   DB_PATH=~/Library/Application\ Support/com.bibliogenius.app/bibliogenius.db \
#     ./scripts/qa_delta_relay.sh <scenario>
#
# Scenarios :
#   delete         scenario suppression (op=delete / tombstone)
#   reset          scenario reset_required (cursor iPhone prune)
#   legacy         instructions pour scenario peer legacy (non scriptable)
#   help           ce message

set -u

DB_PATH="${DB_PATH:-$HOME/Library/Application Support/com.bibliogenius.app/bibliogenius.db}"

pass=0
fail=0

section() { printf '\n\033[1;34m=== %s ===\033[0m\n' "$1"; }
ok()      { printf '  \033[32mOK\033[0m %s\n' "$1"; pass=$((pass + 1)); }
ko()      { printf '  \033[31mKO\033[0m %s\n' "$1"; fail=$((fail + 1)); }
info()    { printf '  .  %s\n' "$1"; }
watch()   { printf '  \033[33m>>\033[0m %s\n' "$1"; }
pause()   { read -r -p "  [ENTER] quand $1" _; }

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing dep: $1" >&2
    exit 2
  }
}

preflight() {
  need sqlite3
  if [[ ! -r "$DB_PATH" ]]; then
    echo "DB_PATH introuvable ou illisible : $DB_PATH" >&2
    exit 1
  fi
  info "DB_PATH = $DB_PATH"
  local nb
  nb=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM books WHERE owned=1;")
  info "Mac library : $nb livres owned"
}

# ── scenario suppression ────────────────────────────────────────────────
scenario_delete() {
  section "Scenario delete : suppression via tombstone"
  preflight

  # Crée un livre sacrificiel horodate, loggue l'INSERT, pousse le nudge
  # en touchant updated_at sur la lib (la notif est declenchee par les
  # handlers HTTP ; ici on patche operation_log manuellement puis on
  # demande a l'utilisateur d'ajouter / supprimer via l'UI pour que le
  # catalog_changed parte vraiment par WebSocket).
  local marker="QA-Relay-Delete-$(date +%s)"
  info "Etape 1/3 : ajoute le livre '$marker' via l'UI Mac, puis attends la synchro cote iPhone"
  watch "Logs iPhone attendus : 'PeerBookList: catalog changed ... attempting delta sync (ADR-029)' puis 'delta applied by Rust'"
  pause "le livre est visible cote iPhone"

  local book_id
  book_id=$(sqlite3 "$DB_PATH" "SELECT id FROM books WHERE title='$marker' ORDER BY id DESC LIMIT 1;")
  if [[ -z "$book_id" ]]; then
    ko "livre '$marker' introuvable cote Mac — l'UI ne l'a pas insere"
    return 1
  fi
  ok "book_id cote Mac = $book_id"

  info "Etape 2/3 : supprime le livre via l'UI Mac (poubelle)"
  watch "Logs iPhone attendus : 'delta applied' + operations contenant {op:\"delete\",book_id:$book_id}"
  pause "le livre a disparu de l'UI iPhone"

  # Verif cote Mac : la ligne books est partie + DELETE present dans oplog
  local still_there
  still_there=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM books WHERE id=$book_id;")
  if [[ "$still_there" == "0" ]]; then
    ok "Mac : ligne books supprimee"
  else
    ko "Mac : ligne books toujours presente (UI delete echoue ?)"
  fi

  local tomb
  tomb=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM operation_log WHERE entity_type='book' AND entity_id=$book_id AND operation='DELETE' AND source='local';")
  if [[ "$tomb" -ge "1" ]]; then
    ok "Mac : tombstone DELETE logge dans operation_log ($tomb rows)"
  else
    ko "Mac : aucun tombstone DELETE — le delta relay n'a rien a transporter"
  fi

  info "Etape 3/3 : verif cote iPhone (manuelle)"
  watch "Sur iPhone, la case 'peer_books' ne doit plus contenir ce livre. Recharge manuelle via pull-to-refresh = aucun effet visible."

  section "Delete : $pass OK / $fail KO"
}

# ── scenario reset_required ─────────────────────────────────────────────
scenario_reset() {
  section "Scenario reset_required : cursor iPhone prune cote Mac"
  preflight

  local oldest current
  oldest=$(sqlite3 "$DB_PATH" "SELECT COALESCE(MIN(id), 0) FROM operation_log;")
  current=$(sqlite3 "$DB_PATH" "SELECT COALESCE(MAX(id), 0) FROM operation_log;")
  info "operation_log Mac : [$oldest .. $current]"

  if [[ "$current" -le "$oldest" ]]; then
    ko "log vide ou une seule ligne — impossible de pruner pour simuler un cursor stale"
    return 1
  fi

  # On ne connait pas le last_delta_cursor cote iPhone (c'est stocke dans
  # la DB iPhone). Strategie simple : supprimer TOUT l'oplog Mac sauf la
  # derniere ligne. Apres ca, n'importe quel cursor < MAX(id) envoyera
  # reset_required — y compris celui que l'iPhone a deja.
  info "Etape 1/2 : prune operation_log Mac pour forcer reset_required sur le prochain delta"
  watch "Tu peux annuler tant que tu n'as pas tape 'oui' (le prune est destructif mais reversible tant que l'app Mac ne remplit pas de nouvelles lignes)"
  read -r -p "  Confirmer le prune ? [oui/NON] : " confirm
  if [[ "$confirm" != "oui" ]]; then
    info "annule"
    return 0
  fi

  local before_prune after_prune pruned
  before_prune=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM operation_log;")
  sqlite3 "$DB_PATH" "DELETE FROM operation_log WHERE id < $current;"
  after_prune=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM operation_log;")
  pruned=$((before_prune - after_prune))
  if [[ "$pruned" -gt "0" ]]; then
    ok "prune OK : $pruned lignes supprimees, il reste $after_prune ligne (id=$current)"
  else
    ko "prune n'a rien supprime"
    return 1
  fi

  info "Etape 2/2 : fais une mutation cote Mac (ajout ou modif d'un livre via UI) pour declencher le WS nudge"
  watch "Logs iPhone attendus :"
  watch "  1) 'PeerBookList: catalog changed ... attempting delta sync (ADR-029)'"
  watch "  2) 'ADR-029 delta: peer X non-applied (ResetRequired), ... falling back'"
  watch "  3) 'PeerBookList: delta not applied, running legacy sync'"
  watch "  puis le flow library_manifest_request + library_page_request s'execute"
  pause "le livre apparait cote iPhone (meme via full sync, c'est ce qu'on veut)"

  info "Apres le fallback reussi, l'iPhone devrait avoir adopte un nouveau cursor (max(id) du log Mac).Prochaine nudge = delta normal."

  section "Reset : $pass OK / $fail KO"
}

# ── scenario peer legacy (non scriptable) ───────────────────────────────
scenario_legacy() {
  section "Scenario peer legacy : peer sans ADR-029"
  cat <<'EOF'

  Ce scenario necessite de revenir en arriere sur Mac, donc pas
  scriptable depuis ici. Sequence manuelle :

  1. Sur Mac, note le commit courant :
       git -C ~/Sites/bibliotech/bibliogenius rev-parse HEAD

  2. Rollback temporaire de bibliogenius au commit juste avant ADR-029
     (le commit d'avant b4109a2) :
       git -C ~/Sites/bibliotech/bibliogenius checkout b4109a2^

  3. Relance le backend Mac (Flutter macOS) depuis ce commit. Le Mac
     ne connait plus catalog_delta_request.

  4. Sur iPhone, ajoute un livre sur Mac via l'UI (Mac publie le
     catalog_changed comme avant, pas changement cote WS).

  5. Logs iPhone attendus :
       - 'PeerBookList: catalog changed ... attempting delta sync (ADR-029)'
       - 'ADR-029 delta: peer X transport error' (timeout 90s)
       - 'PeerBookList: delta not applied, running legacy sync'
       - puis library_manifest_request comme avant

  6. Verifie que le livre apparait quand meme sur iPhone apres les ~90s.
     Bandwidth = legacy (pas de gain), mais le fallback fonctionne.

  7. Restaure le commit courant Mac :
       git -C ~/Sites/bibliotech/bibliogenius checkout main

  Critique : le 90s d'attente est la penalite attendue pour un peer
  legacy. Si ca te gene en prod reelle, on pourra reduire le timeout
  dans try_send_e2ee (aujourd'hui 90s couvre une cycle pollling complete).

EOF
}

main() {
  local cmd="${1:-help}"
  case "$cmd" in
    delete) scenario_delete ;;
    reset)  scenario_reset ;;
    legacy) scenario_legacy ;;
    help|--help|-h)
      cat <<EOF
Usage : $0 <scenario>

Scenarios :
  delete   suppression d'un livre + verif du tombstone
  reset    prune de l'oplog Mac pour declencher reset_required cote iPhone
  legacy   instructions manuelles (rollback Mac)
  help     ce message
EOF
      ;;
    *)
      echo "Scenario inconnu : $cmd" >&2
      echo "Essaie '$0 help'" >&2
      exit 2
      ;;
  esac
}

main "$@"
