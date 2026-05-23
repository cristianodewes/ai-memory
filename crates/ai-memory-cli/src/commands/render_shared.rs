//! Shared rendering helpers for the install-* / setup-agent commands.
//!
//! These three subcommands (`install-hooks`, `install-mcp`,
//! `setup-agent`) all emit configuration snippets that share two
//! pieces of state:
//!
//! 1. The seven Claude Code lifecycle-hook events ai-memory wires
//!    up — kept in sync between hook-bundle generation (setup-agent)
//!    and JSON-config rendering (install-hooks).
//! 2. The optional `Authorization: Bearer <token>` header used by
//!    both MCP client configs (install-mcp) and hook env blocks
//!    (install-hooks / setup-agent).
//!
//! Each subcommand still owns its per-client output formatting (the
//! commentary that frames the JSON snippet differs from client to
//! client and is the part that makes the printout readable). What
//! lives here is only the *data* both consume.

use std::path::Path;

use serde_json::json;

/// Claude Code lifecycle events ai-memory hooks. Each pair is
/// `(event-name-in-Claude-Code-settings, hook-script-filename)`.
///
/// Adding a hook event means updating this list AND adding the
/// matching `hooks/{claude-code,codex,opencode}/<filename>` script —
/// the e2e test + the generator in `bin/regen-hooks` (if added) both
/// key off this constant.
pub(crate) const CLAUDE_CODE_EVENTS: [(&str, &str); 7] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
];

/// Format an `Authorization: Bearer <token>` header value, or `None`
/// when no token is supplied. Used by every MCP client renderer in
/// `install-mcp` and every hook-config renderer that wants to
/// embed an auth token.
///
/// Centralised because the prefix is `Bearer` per RFC 7235 / OAuth
/// 2.1 / the MCP spec — if anyone ever decides to support a
/// different scheme (e.g. `DPoP`) this is the one place that
/// changes.
#[must_use]
pub(crate) fn bearer_header_value(token: Option<&str>) -> Option<String> {
    token.map(|t| format!("Bearer {t}"))
}

/// Build the Claude Code `settings.json` fragment that wires the
/// seven hooks. Used by both:
/// - `install-hooks --agent claude-code` (script paths are
///   wherever the user told us via `--hooks-dir`)
/// - `setup-agent --agent claude-code` (script paths are where
///   `--host-prefix` says they'll live on the host)
///
/// `emit_root` is the directory that will contain `*.sh`; it is
/// expected to be an absolute path on the system that will run the
/// agent CLI. This function does NOT verify the path exists on the
/// local filesystem — that decision belongs to the caller because
/// the docker case legitimately renders host paths that don't yet
/// exist in the container.
///
/// `auth_token`, when set, lands in each hook's `env` block as
/// `AI_MEMORY_AUTH_TOKEN`, which the shell scripts forward as
/// `Authorization: Bearer …` to the server.
#[must_use]
pub(crate) fn build_claude_code_payload(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    let mut hooks_block = serde_json::Map::new();
    for (event, script) in CLAUDE_CODE_EVENTS {
        let abs = emit_root.join(script);
        let mut env = serde_json::Map::new();
        env.insert("AI_MEMORY_HOOK_URL".into(), json!(server_url));
        if let Some(t) = auth_token {
            env.insert("AI_MEMORY_AUTH_TOKEN".into(), json!(t));
        }
        // Claude Code's hook schema:
        //   "<EventName>": [
        //     { "matcher": "<tool-name regex or empty>",
        //       "hooks": [ { "type": "command", "command": "...", "env": {...} } ]
        //     }
        //   ]
        // The OUTER array carries `matcher` + a nested `hooks` array.
        // The INNER hook entry carries `type: "command"` plus the
        // actual command. An empty matcher means "fire for every
        // event of this kind" — appropriate for ai-memory's
        // capture hooks which want every tool call, every prompt,
        // every session boundary.
        hooks_block.insert(
            event.into(),
            json!([{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": abs.to_string_lossy().into_owned(),
                    "env": env,
                }],
            }]),
        );
    }
    json!({ "hooks": hooks_block })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bearer_header_is_none_when_no_token() {
        assert!(bearer_header_value(None).is_none());
    }

    #[test]
    fn bearer_header_prefixes_with_bearer() {
        let h = bearer_header_value(Some("abc123")).unwrap();
        assert_eq!(h, "Bearer abc123");
    }

    #[test]
    fn claude_code_payload_has_seven_events() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let hooks = v.get("hooks").and_then(|h| h.as_object()).unwrap();
        assert_eq!(hooks.len(), 7);
        for (event, _) in CLAUDE_CODE_EVENTS {
            assert!(hooks.contains_key(event), "missing event {event}");
        }
    }

    #[test]
    fn claude_code_payload_embeds_auth_token_when_provided() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", Some("tok"));
        // Updated for Claude Code's matcher+hooks shape: env lives
        // on the INNER hook entry now (one nesting level deeper).
        let session_start = v
            .pointer("/hooks/SessionStart/0/hooks/0/env/AI_MEMORY_AUTH_TOKEN")
            .and_then(|s| s.as_str())
            .unwrap();
        assert_eq!(session_start, "tok");
    }

    /// Regression guard: Claude Code's hook schema requires the
    /// outer array entries to have `matcher` + a nested `hooks`
    /// array (containing the actual `type: "command"` payload).
    /// We shipped the wrong shape briefly — bare `command` at the
    /// outer level — which made Claude Code refuse to load
    /// settings.json with "hooks: Expected array, but received
    /// undefined" on every event.
    #[test]
    fn claude_code_payload_uses_matcher_plus_inner_hooks_shape() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        for (event, _) in CLAUDE_CODE_EVENTS {
            let outer = v
                .pointer(&format!("/hooks/{event}/0"))
                .and_then(|s| s.as_object())
                .unwrap_or_else(|| panic!("missing /hooks/{event}/0"));
            assert!(outer.contains_key("matcher"), "{event}: missing matcher");
            let inner = outer
                .get("hooks")
                .and_then(|h| h.as_array())
                .unwrap_or_else(|| panic!("{event}: missing inner hooks array"));
            assert_eq!(inner.len(), 1);
            let entry = inner[0].as_object().unwrap();
            assert_eq!(
                entry.get("type").and_then(|t| t.as_str()),
                Some("command"),
                "{event}: inner entry must have type: command"
            );
            assert!(
                entry.contains_key("command"),
                "{event}: inner entry missing command"
            );
        }
    }

    #[test]
    fn claude_code_payload_omits_auth_token_when_absent() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        // The env block lives inside the INNER hook entry (one level
        // deeper than the legacy shape).
        let env = v
            .pointer("/hooks/SessionStart/0/hooks/0/env")
            .and_then(|e| e.as_object())
            .unwrap();
        assert!(env.contains_key("AI_MEMORY_HOOK_URL"));
        assert!(!env.contains_key("AI_MEMORY_AUTH_TOKEN"));
    }

    #[test]
    fn claude_code_payload_emits_absolute_paths() {
        let root = PathBuf::from("/home/user/.ai-memory/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert_eq!(
            cmd,
            "/home/user/.ai-memory/hooks/claude-code/session-start.sh"
        );
    }
}
