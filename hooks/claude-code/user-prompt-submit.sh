#!/bin/sh
# Claude Code user-prompt hook.
# Forwards the event JSON to the ai-memory server, fire-and-forget.
# Walks up from the payload's cwd for a .ai-memory.toml marker file;
# if found, appends marker query params to the URL so the server
# applies the declared workspace/project/strategy instead of
# bucketing by basename(cwd) under the default workspace.
# At runtime (after `install-hooks --apply`) `_lib.sh` is staged
# alongside this script. From the source tree it lives one dir up.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=user-prompt&agent=claude-code${QS}" >/dev/null 2>&1 || true

# Opt-in prompt-time recall injection (default OFF). When
# AI_MEMORY_INJECT_RECALL is set, synchronously fetch memory possibly
# relevant to this prompt and prepend it to the agent's context via
# hookSpecificOutput.additionalContext — the same injection seam
# session-start uses for the handoff. This converts recall from a
# voluntary tool call into automatic context. The GET has a 0.5s cap and
# returns an empty body on miss/timeout, so a turn is never delayed.
if [ -n "${AI_MEMORY_INJECT_RECALL:-}" ]; then
    PROMPT=$(ai_memory_extract_prompt "$PAYLOAD")
    if [ -n "$PROMPT" ]; then
        Q=$(ai_memory_url_encode "$(printf '%s' "$PROMPT" | cut -c1-200)")
        RECALL=$(ai_memory_get_recall "$SERVER/recall?q=${Q}${QS}" 2>/dev/null || true)
        if [ -n "$RECALL" ]; then
            printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":%s}}\n' \
                "$(printf '%s' "$RECALL" | ai_memory_json_string)"
            exit 0
        fi
    fi
fi

# Default path: acknowledge with an empty JSON object. Without "{" on
# stdout Claude Code treats the (empty) output as plain text and logs
# "Hook output does not start with {, treating as plain text" on every
# prompt. Emitting {} keeps the debug log clean.
printf '{}\n'
exit 0
