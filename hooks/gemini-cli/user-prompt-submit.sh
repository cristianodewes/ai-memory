#!/bin/sh
# gemini-cli user-prompt hook.
# Forwards the event JSON to the ai-memory server, fire-and-forget.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=user-prompt&agent=gemini-cli${QS}" >/dev/null 2>&1 || true

# Opt-in prompt-time recall injection (default OFF). Mirrors this agent's
# session-start handoff contract: emit recall Markdown as raw stdout, which
# the harness prepends to the turn's context. 0.5s cap; empty body on miss.
if [ -n "${AI_MEMORY_INJECT_RECALL:-}" ]; then
    PROMPT=$(ai_memory_extract_prompt "$PAYLOAD")
    if [ -n "$PROMPT" ]; then
        Q=$(ai_memory_url_encode "$(printf '%s' "$PROMPT" | cut -c1-200)")
        ai_memory_get_recall "$SERVER/recall?q=${Q}${QS}" 2>/dev/null || true
    fi
fi
exit 0
