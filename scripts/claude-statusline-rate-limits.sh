#!/bin/sh
# Persist Claude Code's documented rate-limit status-line fields locally.
set -eu

input=$(cat) || exit 0
session_id=$(printf '%s' "$input" | jq -er '
  .session_id | strings | select(length > 0 and length <= 128)
    | select(test("^[A-Za-z0-9][A-Za-z0-9._-]*$"))
' 2>/dev/null) || exit 0

record=$(printf '%s' "$input" | jq -ce --arg session_id "$session_id" --arg observed_at "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" '
  def window:
    . as $value
    | if ($value | type) == "object"
         and ($value.used_percentage | type) == "number"
         and $value.used_percentage >= 0 and $value.used_percentage <= 100
         and ($value | has("resets_at"))
         and ($value.resets_at | type) == "number"
         and ($value.resets_at | floor) == $value.resets_at
      then {used_percentage: $value.used_percentage, resets_at: ($value.resets_at // null)}
      else null
      end;
  {
    session_id: $session_id,
    prompt_id: (if (.prompt_id | type) == "string" then .prompt_id else null end),
    cost: {
      total_api_duration_ms:
        (if (.cost.total_api_duration_ms | type) == "number"
            and (.cost.total_api_duration_ms | floor) == .cost.total_api_duration_ms
            and .cost.total_api_duration_ms >= 0
         then .cost.total_api_duration_ms else null end)
    },
    rate_limits: {
      five_hour: (.rate_limits.five_hour | window),
      seven_day: (.rate_limits.seven_day | window)
    },
    observed_at: $observed_at
  }
  | select(.rate_limits.five_hour != null or .rate_limits.seven_day != null)
' 2>/dev/null) || {
  printf '%s\n' 'Claude rate limits unavailable'
  exit 0
}

line=$(printf '%s' "$record" | jq -r '
  [.rate_limits.five_hour, .rate_limits.seven_day]
  | to_entries
  | map(select(.value != null)
      | (if .key == 0 then "5h " else "7d " end)
        + (.value.used_percentage | tostring) + "% used")
  | join(" · ")
' 2>/dev/null)

if [ -n "${XDG_CACHE_HOME:-}" ]; then
  cache_root=$XDG_CACHE_HOME
elif [ -n "${HOME:-}" ]; then
  cache_root=$HOME/.cache
else
  cache_root=
fi
if [ -n "$cache_root" ]; then
  cache_dir=$cache_root/codex-agent-monitor/claude-rate-limits
  if mkdir -p "$cache_dir" 2>/dev/null; then
    target=$cache_dir/$session_id.json
    new_fingerprint=$(printf '%s' "$record" | jq -c '[.prompt_id, .cost.total_api_duration_ms, .rate_limits]' 2>/dev/null || :)
    old_fingerprint=$(jq -c '[.prompt_id, .cost.total_api_duration_ms, .rate_limits]' "$target" 2>/dev/null || :)
    if [ "$new_fingerprint" != "$old_fingerprint" ]; then
      temporary=$cache_dir/.$session_id.$$.tmp
      trap 'rm -f "$temporary"' 0 HUP INT TERM
      if printf '%s\n' "$record" > "$temporary" && mv -f "$temporary" "$target"; then
        trap - 0 HUP INT TERM
      fi
    fi
  fi
fi

printf 'Claude %s\n' "$line"
