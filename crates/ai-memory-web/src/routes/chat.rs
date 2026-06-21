//! Chat over the configured LLM, grounded in the wiki.
//!
//! Three surfaces, all under the `/web` router:
//! - `POST /w/:workspace/:project/chat` — folder-scoped chat for one
//!   project. The model may also propose page mutations: `create`/`edit`
//!   are applied immediately through the sanitized wiki write path;
//!   `delete` is deferred and returned as a pending action the UI must
//!   confirm via the endpoint below.
//! - `POST /w/:workspace/:project/chat/delete` — apply a confirmed
//!   delete (the only mutation that needs human approval).
//! - `POST /chat` — read-only chat across **all** projects (the home
//!   page's "all projects" button). No mutations here.
//!
//! Reads stay the source-of-truth markdown; every mutation goes through
//! [`ai_memory_wiki::Wiki`] so sanitization, attribution, git history,
//! and the SQLite index stay consistent. Context assembly is hybrid:
//! an outline plus the bodies of the pages most relevant to the latest
//! question (embedding cosine, BM25 fallback).

use std::sync::Arc;

use ai_memory_core::{ActorContext, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{ChatMessage, ChatRequest, Role, Usage, complete_structured};
use ai_memory_store::{PageSummary, ResolvedScope, ScopeResolutionError, lookup_existing_scope};
use ai_memory_wiki::WritePageRequest;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::WebState;

const MAX_TURNS: usize = 40;
const MAX_TURN_CHARS: usize = 16_000;
const TOP_K: usize = 6;
const PER_PAGE_BODY_CHARS: usize = 12_000;
const TOTAL_BODY_CHARS: usize = 60_000;
const MAX_OUTLINE_PAGES: usize = 500;
const MAX_COMPLETION_TOKENS: u32 = 2048;
/// Cap how many pages a single body of the maximum size can produce.
const MAX_ACTIONS: usize = 12;

// ---------------------------------------------------------------------------
// Request / response bodies
// ---------------------------------------------------------------------------

/// Body for `POST …/chat` (project-scoped).
#[derive(Debug, Deserialize)]
pub(crate) struct ChatBody {
    #[serde(default)]
    folder: String,
    messages: Vec<ChatMessage>,
}

/// Body for `POST /chat` (all-projects, read-only).
#[derive(Debug, Deserialize)]
pub(crate) struct GlobalChatBody {
    messages: Vec<ChatMessage>,
}

/// Body for `POST …/chat/delete` (confirmed delete).
#[derive(Debug, Deserialize)]
pub(crate) struct DeleteBody {
    path: String,
}

/// Structured assistant turn for the writable project chat: a reply plus
/// any page mutations the model wants to perform.
#[derive(Debug, Deserialize, JsonSchema)]
struct ChatTurn {
    /// Natural-language answer (Markdown allowed).
    reply: String,
    /// Page mutations to perform this turn; empty for pure Q&A.
    actions: Vec<ChatAction>,
}

/// One proposed page mutation. All fields are required (strict JSON
/// schema); `title`/`body` are empty strings for a `delete`.
#[derive(Debug, Deserialize, JsonSchema)]
struct ChatAction {
    /// `create`, `edit`, or `delete`.
    op: String,
    /// Page path relative to the project, e.g. `decisions/foo.md`.
    path: String,
    /// Page title for `create`/`edit` (empty to derive from the body).
    title: String,
    /// Full Markdown body for `create`/`edit` (empty for `delete`).
    body: String,
    /// One-line reason shown to the user.
    reason: String,
}

#[derive(Debug, Serialize)]
struct AppliedAction {
    op: String,
    path: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct PendingDelete {
    path: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct ChatReply {
    reply: String,
    model: String,
    context_pages: Vec<String>,
    context_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    applied: Vec<AppliedAction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pending_deletes: Vec<PendingDelete>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
}

// ---------------------------------------------------------------------------
// Project-scoped chat (writable)
// ---------------------------------------------------------------------------

/// Handler for `POST /w/:workspace/:project/chat`.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<ChatBody>,
) -> Result<Response, Response> {
    let Some(llm) = state.llm.clone() else {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "chat is unavailable: this server has no LLM provider configured",
        ));
    };
    let turns = validate_turns(body.messages).map_err(into_response)?;
    let question = latest_user_question(&turns)
        .map_err(into_response)?
        .to_owned();
    let folder = body.folder.trim().trim_matches('/').to_owned();

    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;

    let pages = state
        .reader
        .list_pages(&workspace, &project)
        .await
        .map_err(internal_error)?;
    let scoped: Vec<PageSummary> = pages
        .into_iter()
        .filter(|p| in_scope(&p.path, &folder))
        .collect();
    if scoped.is_empty() {
        return Err(json_error(
            StatusCode::NOT_FOUND,
            folder_empty_message(&folder),
        ));
    }

    let ranked = rank_pages(&state, workspace_id, project_id, &scoped, &question).await;
    let (bodies, context_pages, context_truncated) =
        load_bodies(&state, workspace_id, project_id, &scoped, &ranked).await;

    let request = ChatRequest {
        system: Some(build_system_prompt(
            &workspace, &project, &folder, &scoped, &bodies, true,
        )),
        messages: turns.clone(),
        max_tokens: MAX_COMPLETION_TOKENS,
        temperature: None,
    };

    // The action path needs structured output. Every provider implements it,
    // but a weak OpenAI-compatible model can emit malformed JSON; rather than
    // hard-fail, degrade to a plain read-only reply so the chat still answers
    // on any configured LLM (it just can't propose edits in that fallback).
    let turn: ChatTurn = match complete_structured(llm.as_ref(), request).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "web chat: structured output failed; falling back to a read-only reply");
            let fallback = ChatRequest {
                system: Some(build_system_prompt(
                    &workspace, &project, &folder, &scoped, &bodies, false,
                )),
                messages: turns,
                max_tokens: MAX_COMPLETION_TOKENS,
                temperature: None,
            };
            let resp = llm.complete(fallback).await.map_err(|e2| {
                tracing::warn!(error = %e2, "web chat: provider call failed");
                json_error(StatusCode::BAD_GATEWAY, "the LLM provider call failed")
            })?;
            ChatTurn {
                reply: resp.text,
                actions: Vec::new(),
            }
        }
    };

    // Apply create/edit immediately; defer delete for confirmation.
    let mut applied = Vec::new();
    let mut pending_deletes = Vec::new();
    for action in turn.actions.into_iter().take(MAX_ACTIONS) {
        match action.op.to_ascii_lowercase().as_str() {
            "create" | "edit" => {
                let op = action.op.to_ascii_lowercase();
                let path = action.path.clone();
                match apply_write(&state, workspace_id, project_id, action).await {
                    Ok(()) => applied.push(AppliedAction {
                        op,
                        path,
                        ok: true,
                        error: None,
                    }),
                    Err(e) => applied.push(AppliedAction {
                        op,
                        path,
                        ok: false,
                        error: Some(e),
                    }),
                }
            }
            "delete" => pending_deletes.push(PendingDelete {
                path: action.path,
                reason: action.reason,
            }),
            _ => {}
        }
    }

    Ok(Json(ChatReply {
        reply: turn.reply,
        model: llm.model().to_owned(),
        context_pages,
        context_truncated,
        applied,
        pending_deletes,
        usage: None,
    })
    .into_response())
}

/// Apply a `create`/`edit` through the sanitized wiki write path.
async fn apply_write(
    state: &WebState,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    action: ChatAction,
) -> Result<(), String> {
    let path = PagePath::new(&action.path).map_err(|e| format!("invalid path: {e}"))?;
    if action.body.trim().is_empty() {
        return Err("body must not be empty for create/edit".to_owned());
    }
    let title = if action.title.trim().is_empty() {
        None
    } else {
        Some(action.title)
    };
    let req = WritePageRequest {
        workspace_id,
        project_id,
        path,
        frontmatter: serde_json::json!({ "kind": "note" }),
        body: action.body,
        tier: Tier::Semantic,
        pinned: false,
        title,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::anonymous(),
    };
    state
        .wiki
        .write_page(req)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Handler for `POST /w/:workspace/:project/chat/delete` — apply a
/// user-confirmed delete.
pub(crate) async fn delete_handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<DeleteBody>,
) -> Result<Response, Response> {
    // Deletes only make sense when chat (the only producer of pending
    // deletes) is enabled; gate on the same LLM presence.
    if state.llm.is_none() {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "chat is unavailable: this server has no LLM provider configured",
        ));
    }
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let path = PagePath::new(body.path.trim())
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("invalid path: {e}")))?;
    state
        .wiki
        .delete_page(workspace_id, project_id, &path, None)
        .await
        .map_err(|e| json_error(StatusCode::BAD_GATEWAY, format!("delete failed: {e}")))?;
    Ok(Json(serde_json::json!({ "ok": true, "path": path.as_str() })).into_response())
}

// ---------------------------------------------------------------------------
// Global (all-projects) chat — read-only
// ---------------------------------------------------------------------------

/// Handler for `POST /chat` — chat grounded in every project. Read-only.
pub(crate) async fn global_handler(
    State(state): State<Arc<WebState>>,
    Json(body): Json<GlobalChatBody>,
) -> Result<Response, Response> {
    let Some(llm) = state.llm.clone() else {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "chat is unavailable: this server has no LLM provider configured",
        ));
    };
    let turns = validate_turns(body.messages).map_err(into_response)?;
    let question = latest_user_question(&turns)
        .map_err(into_response)?
        .to_owned();

    let projects = state
        .reader
        .list_projects_with_stats()
        .await
        .map_err(internal_error)?;
    if projects.is_empty() {
        return Err(json_error(
            StatusCode::NOT_FOUND,
            "no projects to chat about yet",
        ));
    }

    // Top-K relevant pages across ALL projects via global FTS, with bodies.
    let hits = state
        .reader
        .search_pages(question.clone(), TOP_K)
        .await
        .map_err(internal_error)?;
    let mut bodies = Vec::new();
    let mut context_pages = Vec::new();
    let mut budget = TOTAL_BODY_CHARS;
    for hit in hits {
        let Some(meta) = state
            .reader
            .page_meta_by_id(hit.id)
            .await
            .map_err(internal_error)?
        else {
            continue;
        };
        let Ok(doc) = state
            .wiki
            .read_page(meta.workspace_id, meta.project_id, &hit.path)
        else {
            continue;
        };
        let cap = PER_PAGE_BODY_CHARS.min(budget);
        if cap == 0 {
            break;
        }
        let (clipped, _) = clip(&doc.body, cap);
        budget = budget.saturating_sub(clipped.chars().count());
        let label = format!(
            "{}/{}:{}",
            meta.workspace_name, meta.project_name, meta.path
        );
        context_pages.push(label.clone());
        bodies.push(BodySection {
            path: label,
            title: meta.title,
            body: clipped,
        });
    }

    let system = build_global_system_prompt(&projects, &bodies);
    let request = ChatRequest {
        system: Some(system),
        messages: turns,
        max_tokens: MAX_COMPLETION_TOKENS,
        temperature: None,
    };
    let response = llm.complete(request).await.map_err(|e| {
        tracing::warn!(error = %e, "web global chat: provider call failed");
        json_error(StatusCode::BAD_GATEWAY, "the LLM provider call failed")
    })?;

    Ok(Json(ChatReply {
        reply: response.text,
        model: response.model,
        context_pages,
        context_truncated: false,
        applied: Vec::new(),
        pending_deletes: Vec::new(),
        usage: response.usage,
    })
    .into_response())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

type ChatError = (StatusCode, String);

fn into_response((status, message): ChatError) -> Response {
    json_error(status, message)
}

fn validate_turns(messages: Vec<ChatMessage>) -> Result<Vec<ChatMessage>, ChatError> {
    if messages.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "messages must contain at least one turn".to_owned(),
        ));
    }
    if messages.len() > MAX_TURNS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("too many turns (max {MAX_TURNS})"),
        ));
    }
    for m in &messages {
        if m.content.chars().count() > MAX_TURN_CHARS {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("a message exceeds the {MAX_TURN_CHARS}-character limit"),
            ));
        }
    }
    Ok(messages)
}

fn latest_user_question(turns: &[ChatMessage]) -> Result<&str, ChatError> {
    turns
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| m.content.trim())
        .filter(|q| !q.is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "the last turn must be a non-empty user message".to_owned(),
            )
        })
}

fn in_scope(path: &str, folder: &str) -> bool {
    if folder.is_empty() {
        return true;
    }
    path == folder || path.starts_with(&format!("{folder}/"))
}

async fn rank_pages(
    state: &WebState,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    scoped: &[PageSummary],
    question: &str,
) -> Vec<String> {
    if let Some(paths) = rank_by_embedding(state, workspace_id, project_id, scoped, question).await
    {
        return paths;
    }
    if let Some(paths) = rank_by_bm25(state, workspace_id, project_id, scoped, question).await {
        return paths;
    }
    let mut by_recency: Vec<&PageSummary> = scoped.iter().collect();
    by_recency.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    by_recency
        .into_iter()
        .take(TOP_K)
        .map(|p| p.path.clone())
        .collect()
}

async fn rank_by_embedding(
    state: &WebState,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    scoped: &[PageSummary],
    question: &str,
) -> Option<Vec<String>> {
    let embedder = state.embedder.as_ref()?;
    let query_vec = match embedder.embed_query(question).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "web chat: query embed failed; falling back to BM25");
            return None;
        }
    };
    let stored = state
        .reader
        .load_embeddings(
            workspace_id,
            project_id,
            embedder.provider().to_owned(),
            embedder.model().to_owned(),
            embedder.dim(),
        )
        .await
        .ok()?;
    if stored.is_empty() {
        return None;
    }
    let in_scope_paths: std::collections::HashSet<&str> =
        scoped.iter().map(|p| p.path.as_str()).collect();
    let mut scored: Vec<(f32, String)> = stored
        .into_iter()
        .filter(|e| in_scope_paths.contains(e.path.as_str()))
        .map(|e| (dot(&query_vec, &e.vector), e.path.as_str().to_owned()))
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(TOP_K);
    Some(scored.into_iter().map(|(_, path)| path).collect())
}

async fn rank_by_bm25(
    state: &WebState,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    scoped: &[PageSummary],
    question: &str,
) -> Option<Vec<String>> {
    let in_scope_paths: std::collections::HashSet<&str> =
        scoped.iter().map(|p| p.path.as_str()).collect();
    let hits = state
        .reader
        .search_pages_for_project(workspace_id, project_id, question.to_owned(), TOP_K * 4)
        .await
        .ok()?;
    let paths: Vec<String> = hits
        .into_iter()
        .map(|h| h.path.as_str().to_owned())
        .filter(|p| in_scope_paths.contains(p.as_str()))
        .take(TOP_K)
        .collect();
    (!paths.is_empty()).then_some(paths)
}

async fn load_bodies(
    state: &WebState,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    scoped: &[PageSummary],
    ranked: &[String],
) -> (Vec<BodySection>, Vec<String>, bool) {
    let mut sections = Vec::new();
    let mut included = Vec::new();
    let mut truncated = false;
    let mut budget = TOTAL_BODY_CHARS;

    for path in ranked {
        if budget == 0 {
            truncated = true;
            break;
        }
        let Ok(page_path) = PagePath::new(path) else {
            continue;
        };
        let Ok(doc) = state.wiki.read_page(workspace_id, project_id, &page_path) else {
            continue;
        };
        let title = scoped
            .iter()
            .find(|p| &p.path == path)
            .map(|p| p.title.clone())
            .unwrap_or_default();
        let cap = PER_PAGE_BODY_CHARS.min(budget);
        let (body, was_cut) = clip(&doc.body, cap);
        truncated |= was_cut;
        budget = budget.saturating_sub(body.chars().count());
        included.push(path.clone());
        sections.push(BodySection {
            path: path.clone(),
            title,
            body,
        });
    }
    (sections, included, truncated)
}

struct BodySection {
    path: String,
    title: String,
    body: String,
}

fn build_system_prompt(
    workspace: &str,
    project: &str,
    folder: &str,
    scoped: &[PageSummary],
    bodies: &[BodySection],
    writable: bool,
) -> String {
    use std::fmt::Write as _;

    let scope_label = if folder.is_empty() {
        format!("the whole project \"{project}\"")
    } else {
        format!("the folder \"{folder}/\" (and its sub-folders) of project \"{project}\"")
    };

    let mut s = String::new();
    if writable {
        let _ = write!(
            s,
            "You are a memory editor for the ai-memory wiki, working in {scope_label}, \
             workspace \"{workspace}\". You have WRITE access to this project's pages and can \
             change memory directly — you are NOT read-only.\n\n\
             When the user asks you to add, update, correct, rename, or remove memory, perform \
             it by returning entries in the `actions` array. Never reply that you cannot edit or \
             that you are read-only — use the actions instead:\n\
             - `create`: a new page. Set `path` (e.g. `decisions/foo.md`), `title`, and the full \
             Markdown `body`.\n\
             - `edit`: replace an existing page's entire body. Set `path`, `title`, and the \
             complete new `body` (a full replacement, not a diff).\n\
             - `delete`: remove a page. Set `path` and a `reason`; leave `title` and `body` empty.\n\
             `create` and `edit` take effect immediately; `delete` is applied only after the user \
             confirms in the UI, so explain the deletion in your `reply`. For a pure question \
             (no change requested), return an empty `actions` array and answer in `reply`. Keep \
             every `path` inside this project.\n\n\
             Ground factual claims in the pages below and cite the page paths you rely on. Be \
             concise; format `reply` with Markdown.\n\n"
        );
    } else {
        let _ = write!(
            s,
            "You are a memory analyst for the ai-memory wiki, working in {scope_label}, \
             workspace \"{workspace}\". Ground every claim in the pages below and cite the \
             page paths you rely on. If the pages do not answer the question, say so plainly. \
             Be concise; format with Markdown.\n\n"
        );
    }

    s.push_str("# Pages in scope (outline)\n");
    for p in scoped.iter().take(MAX_OUTLINE_PAGES) {
        let _ = writeln!(s, "- {} — {} [{}]", p.path, p.title, p.kind);
    }
    if scoped.len() > MAX_OUTLINE_PAGES {
        let _ = writeln!(s, "- …and {} more pages", scoped.len() - MAX_OUTLINE_PAGES);
    }

    if !bodies.is_empty() {
        s.push_str("\n# Most relevant page bodies\n");
        for b in bodies {
            let _ = write!(s, "\n## {} — {}\n{}\n", b.path, b.title, b.body);
        }
    }
    s
}

fn build_global_system_prompt(
    projects: &[ai_memory_store::ProjectSummary],
    bodies: &[BodySection],
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str(
        "You are a memory analyst for the ai-memory wiki, with read access to EVERY project. \
         Ground every claim in the pages below and cite the `workspace/project:path` of what \
         you use. If the pages do not answer the question, say so plainly. Be concise; format \
         with Markdown. This is a read-only view: do not claim to have changed anything.\n\n",
    );
    s.push_str("# Projects (workspace/project — pages)\n");
    for p in projects.iter().take(MAX_OUTLINE_PAGES) {
        let _ = writeln!(
            s,
            "- {}/{} — {} pages",
            p.workspace_name, p.project_name, p.page_count
        );
    }
    if !bodies.is_empty() {
        s.push_str("\n# Most relevant page bodies\n");
        for b in bodies {
            let _ = write!(s, "\n## {} — {}\n{}\n", b.path, b.title, b.body);
        }
    }
    s
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn clip(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text.to_owned(), false);
    }
    let clipped: String = text.chars().take(max).collect();
    (clipped, true)
}

fn folder_empty_message(folder: &str) -> String {
    if folder.is_empty() {
        "this project has no pages to chat about yet".to_owned()
    } else {
        format!("no pages found under folder '{folder}'")
    }
}

async fn lookup_project(
    state: &WebState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), Response> {
    lookup_existing_scope(&state.reader, workspace, project)
        .await
        .map(ResolvedScope::as_tuple)
        .map_err(|err| match err {
            ScopeResolutionError::ProjectNotFoundInWorkspace { project, .. } => json_error(
                StatusCode::NOT_FOUND,
                format!("project '{project}' not found"),
            ),
            other if other.is_bad_request() => {
                json_error(StatusCode::BAD_REQUEST, other.to_string())
            }
            other if other.is_not_found() => json_error(StatusCode::NOT_FOUND, other.to_string()),
            other => internal_error(other),
        })
}

fn internal_error(e: impl std::fmt::Display) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_scope_matches_folder_index_and_descendants_only() {
        assert!(in_scope("decisions", "decisions"));
        assert!(in_scope("decisions/auth", "decisions"));
        assert!(in_scope("decisions/auth/oauth", "decisions"));
        assert!(!in_scope("decisions-archive", "decisions"));
        assert!(!in_scope("gotchas/x", "decisions"));
        assert!(in_scope("anything/at/all", ""));
    }

    #[test]
    fn dot_handles_mismatched_lengths() {
        assert_eq!(dot(&[1.0, 0.0], &[1.0, 0.0]), 1.0);
        assert_eq!(dot(&[1.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn clip_truncates_on_char_boundary() {
        let (s, cut) = clip("héllo wörld", 5);
        assert!(cut);
        assert_eq!(s.chars().count(), 5);
        let (s, cut) = clip("short", 50);
        assert!(!cut);
        assert_eq!(s, "short");
    }
}
