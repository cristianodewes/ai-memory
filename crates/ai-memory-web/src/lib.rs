//! `ai-memory-web` — HTTP browser for the wiki.
//!
//! Mounted under `/web` on the same axum server that hosts the MCP
//! endpoint, so a single port + single auth posture covers both. The
//! browsing surface is read-only — no editing, no agent-write APIs —
//! the wiki is already markdown-on-disk and this just makes it
//! browsable from a phone, a tablet, or a teammate's machine without
//! `docker exec cat …`. The one interactive surface is the
//! folder-scoped chat (`POST …/chat`): it never mutates the wiki, it
//! only feeds a folder's pages to the configured LLM as grounding.
//!
//! Routes (all under whatever prefix the host nests this router at):
//! - `GET  /`                              → project list (cards)
//! - `GET  /w/:workspace/:project`         → page tree + recent activity
//! - `GET  /w/:workspace/:project/p/*path` → rendered markdown + metadata
//! - `POST /w/:workspace/:project/chat`    → folder-scoped LLM chat (JSON)
//! - `GET  /search?q=…`                    → FTS5 hit list
//! - `GET  /static/*`                      → embedded CSS + JS + logo
//!
//! The companion `api_router` exposes the read-only data as JSON for
//! custom frontends. It intentionally does not expose write/admin
//! operations, nor the chat (which needs an LLM the SPA host wires
//! itself); pass `None` providers there.
//!
//! Theme follows `prefers-color-scheme` via the included Tailwind
//! stylesheet; the only JS is the small chat client.

use std::sync::Arc;

use ai_memory_llm::{Embedder, LlmProvider};
use ai_memory_store::ReaderPool;
use ai_memory_wiki::Wiki;
use axum::Router;

mod markdown;
mod routes;
mod state;
mod templates;

pub use state::WebState;

/// Build the web router. Call once at server startup and
/// `nest("/web", router)` it onto the existing axum app, OR mount at
/// `/` if the web UI is the only HTTP surface.
///
/// `llm` and `embedder` power the folder-scoped chat. Pass `None` for
/// either to degrade gracefully: `llm = None` disables chat entirely
/// (route answers `503`, UI hides the affordance); `embedder = None`
/// falls back to BM25 ranking when assembling the chat context.
pub fn router(
    reader: ReaderPool,
    wiki: Wiki,
    llm: Option<Arc<dyn LlmProvider>>,
    embedder: Option<Arc<dyn Embedder>>,
) -> Router {
    let state = Arc::new(WebState::new(reader, wiki, llm, embedder));
    routes::build(state)
}

/// Build the read-only JSON API router for third-party web UIs.
///
/// The host should `nest("/api/v1", api_router(...))` alongside `/web`
/// so custom frontends can browse memory without reading SQLite or wiki
/// files directly. This surface carries no LLM, so chat is not exposed
/// here.
pub fn api_router(reader: ReaderPool, wiki: Wiki) -> Router {
    let state = Arc::new(WebState::new(reader, wiki, None, None));
    routes::build_api(state)
}

/// Standalone `GET /favicon.ico` router. Merge at the host-root level
/// (NOT under `--base-path`, NOT nested under `/web`) so the browser's
/// automatic `/favicon.ico` fetch actually reaches it. The handler is
/// stateless — it returns the same embedded PNG as `/web/static/logo.png`.
pub fn favicon_router() -> Router {
    routes::build_favicon()
}
