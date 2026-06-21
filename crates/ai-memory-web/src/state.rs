//! Web router state — the handle a request handler receives.
//!
//! Holds the read-only store pool + the wiki handle, plus the optional
//! LLM provider + embedder that power the folder-scoped chat. Cheap to
//! clone (everything inside is `Arc`-shaped already), so axum's
//! `State<Arc<WebState>>` extractor stays free of clone-heavy code.

use std::sync::Arc;

use ai_memory_llm::{Embedder, LlmProvider};
use ai_memory_store::ReaderPool;
use ai_memory_wiki::Wiki;

/// Shared state for every web route. Construct once via
/// [`crate::router`].
#[derive(Clone)]
pub struct WebState {
    /// Read-only SQLite pool — drives FTS5 search, page metadata,
    /// project list aggregates.
    pub reader: ReaderPool,
    /// Wiki handle — reads page bodies from disk.
    pub wiki: Wiki,
    /// Chat completion provider. `None` when the server has no LLM
    /// configured — the chat route then answers `503` and the UI hides
    /// its chat affordances.
    pub llm: Option<Arc<dyn LlmProvider>>,
    /// Embedder used to rank a folder's pages against the user's
    /// question (semantic top-K). `None` degrades the chat context
    /// retrieval to BM25-only.
    pub embedder: Option<Arc<dyn Embedder>>,
}

impl WebState {
    /// Build a new shared state.
    #[must_use]
    pub fn new(
        reader: ReaderPool,
        wiki: Wiki,
        llm: Option<Arc<dyn LlmProvider>>,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Self {
        Self {
            reader,
            wiki,
            llm,
            embedder,
        }
    }
}
