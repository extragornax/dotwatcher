#!/usr/bin/env bash
# Create one Grafana public dashboard per currently-cached race in
# madcap_fast, each with the `slug` variable pinned so viewers land on a
# single event without needing the dropdown.
#
# Workflow:
#   1. Fetch the base dashboard (uid: madcap-race) from Grafana.
#   2. List active slugs from the madcap_fast /metrics endpoint.
#   3. For each slug, POST a cloned dashboard (new uid, title, pinned
#      slug, hidden variable) to Grafana.
#   4. Enable its public-dashboard and print the public URL.
#
# Idempotent: re-running upserts the per-slug dashboards (`overwrite: true`)
# and reuses existing public dashboards if already enabled.
#
# Requirements: curl, jq.
#
# Env:
#   MADCAP_FAST_URL   default http://localhost:9004
#   GRAFANA_URL       default http://localhost:9007
#   GRAFANA_USER      default admin
#   GRAFANA_PASSWORD  default admin
#   BASE_UID          default madcap-race
#
# Example:
#   MADCAP_FAST_URL=http://madcap.extragornax.fr \
#   GRAFANA_URL=http://grafana.extragornax.fr \
#   GRAFANA_USER=admin GRAFANA_PASSWORD=secret \
#   ./monitoring/setup-public-dashboards.sh

set -euo pipefail

MADCAP_FAST_URL="${MADCAP_FAST_URL:-http://localhost:9004}"
GRAFANA_URL="${GRAFANA_URL:-http://localhost:9007}"
GRAFANA_USER="${GRAFANA_USER:-admin}"
GRAFANA_PASSWORD="${GRAFANA_PASSWORD:-admin}"
BASE_UID="${BASE_UID:-madcap-race}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing dependency: $1" >&2; exit 1; }; }
need curl
need jq

auth() {
  curl -sS --fail-with-body \
    --user "$GRAFANA_USER:$GRAFANA_PASSWORD" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    "$@"
}

echo ">> base dashboard: $GRAFANA_URL/api/dashboards/uid/$BASE_UID"
base=$(auth "$GRAFANA_URL/api/dashboards/uid/$BASE_UID")
base_dash=$(echo "$base" | jq '.dashboard')
if [[ -z "$base_dash" || "$base_dash" == "null" ]]; then
  echo "base dashboard $BASE_UID not found; start the monitoring stack first" >&2
  exit 1
fi

echo ">> slugs from $MADCAP_FAST_URL/metrics"
slugs=$(curl -sS "$MADCAP_FAST_URL/metrics" \
        | grep -oE 'madcap_event_total_km\{slug="[^"]+' \
        | sed -E 's/.*slug="//' \
        | sort -u)

if [[ -z "$slugs" ]]; then
  echo "no active events found (only live events emit madcap_event_total_km)" >&2
  exit 1
fi

printf 'Per-slug public dashboards\n'
printf '%s\n' '--------------------------------------------------------------'

while IFS= read -r slug; do
  [[ -z "$slug" ]] && continue
  new_uid="madcap-race-${slug}"
  new_title="madcap_fast — ${slug}"

  clone=$(echo "$base_dash" | jq \
    --arg uid "$new_uid" \
    --arg title "$new_title" \
    --arg slug "$slug" '
      .id = null
      | .uid = $uid
      | .title = $title
      | .version = 0
      | .templating.list = (.templating.list | map(
          if .name == "slug" then
            .current = { "selected": false, "text": $slug, "value": $slug }
            | .hide = 2
            | .options = [{ "selected": true, "text": $slug, "value": $slug }]
          else . end
        ))
    ')

  # Upsert dashboard
  payload=$(jq -n --argjson d "$clone" '{dashboard:$d, overwrite:true, message:"per-slug public dashboard"}')
  auth -X POST "$GRAFANA_URL/api/dashboards/db" -d "$payload" >/dev/null

  # Enable public dashboard (ignore error if one already exists)
  pd_json=$(auth -X POST "$GRAFANA_URL/api/dashboards/uid/$new_uid/public-dashboards" \
    -d '{"isEnabled":true,"annotationsEnabled":false,"timeSelectionEnabled":true}' 2>/dev/null || true)
  if [[ -z "$pd_json" ]] || ! echo "$pd_json" | jq -e '.accessToken' >/dev/null 2>&1; then
    pd_json=$(auth "$GRAFANA_URL/api/dashboards/uid/$new_uid/public-dashboards")
  fi
  token=$(echo "$pd_json" | jq -r '.accessToken // empty')
  if [[ -z "$token" ]]; then
    echo "  $slug  FAILED (no accessToken)" >&2
    echo "    response: $pd_json" >&2
    continue
  fi
  echo "  $slug  ->  $GRAFANA_URL/public-dashboards/$token"
done <<< "$slugs"
