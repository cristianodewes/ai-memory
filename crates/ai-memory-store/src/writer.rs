//! Single-writer SQLite actor.
//!
//! Every mutating SQL statement flows through one dedicated OS thread that
//! owns the writer [`rusqlite::Connection`]. Callers send [`WriteCmd`]
//! variants over an mpsc channel and receive results back through a
//! `oneshot`. This pattern eliminates the `database is locked` failure
//! mode that bit cognee (#2717) — there is exactly one writer at all
//! times, by construction.

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use ai_memory_core::{NewPage, PageId, ProjectId, WorkspaceId};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::error::{StoreError, StoreResult};
use crate::ops;

/// Commands accepted by the writer thread.
pub(crate) enum WriteCmd {
    GetOrCreateWorkspace {
        name: String,
        reply: oneshot::Sender<StoreResult<WorkspaceId>>,
    },
    GetOrCreateProject {
        workspace_id: WorkspaceId,
        name: String,
        repo_path: Option<String>,
        reply: oneshot::Sender<StoreResult<ProjectId>>,
    },
    UpsertPage {
        page: NewPage,
        reply: oneshot::Sender<StoreResult<PageId>>,
    },
    Shutdown,
}

/// Cheap, cloneable handle that submits commands to the writer.
#[derive(Clone)]
pub struct WriterHandle {
    inner: Arc<WriterInner>,
}

struct WriterInner {
    tx: mpsc::Sender<WriteCmd>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl WriterHandle {
    /// Take ownership of `conn` and spawn the writer thread.
    pub(crate) fn spawn(conn: Connection) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let handle = thread::Builder::new()
            .name("ai-memory-writer".into())
            .spawn(move || worker_loop(conn, rx))
            .expect("spawn writer thread");

        Self {
            inner: Arc::new(WriterInner {
                tx,
                join: Mutex::new(Some(handle)),
            }),
        }
    }

    /// Resolve a workspace by name, creating it atomically if missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::get_or_create_workspace`].
    pub async fn get_or_create_workspace(
        &self,
        name: impl Into<String>,
    ) -> StoreResult<WorkspaceId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::GetOrCreateWorkspace {
            name: name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Resolve a project by `(workspace_id, name)`, creating it atomically
    /// if missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::get_or_create_project`].
    pub async fn get_or_create_project(
        &self,
        workspace_id: WorkspaceId,
        name: impl Into<String>,
        repo_path: Option<String>,
    ) -> StoreResult<ProjectId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::GetOrCreateProject {
            workspace_id,
            name: name.into(),
            repo_path,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Upsert a page (creating it or superseding the existing latest
    /// version when the body has changed).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::upsert_page`].
    pub async fn upsert_page(&self, page: NewPage) -> StoreResult<PageId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::UpsertPage { page, reply: tx }).await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    async fn send(&self, cmd: WriteCmd) -> StoreResult<()> {
        self.inner
            .tx
            .send(cmd)
            .await
            .map_err(|_| StoreError::WriterClosed)
    }
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        let _ = self.tx.try_send(WriteCmd::Shutdown);
        if let Some(handle) = self.join.lock().expect("writer join mutex poisoned").take() {
            let _ = handle.join();
        }
    }
}

fn worker_loop(mut conn: Connection, mut rx: mpsc::Receiver<WriteCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCmd::Shutdown => break,
            WriteCmd::GetOrCreateWorkspace { name, reply } => {
                let result = ops::get_or_create_workspace(&mut conn, &name);
                let _ = reply.send(result);
            }
            WriteCmd::GetOrCreateProject {
                workspace_id,
                name,
                repo_path,
                reply,
            } => {
                let result = ops::get_or_create_project(
                    &mut conn,
                    &workspace_id,
                    &name,
                    repo_path.as_deref(),
                );
                let _ = reply.send(result);
            }
            WriteCmd::UpsertPage { page, reply } => {
                let result = ops::upsert_page(&mut conn, &page);
                let _ = reply.send(result);
            }
        }
    }
    tracing::debug!("writer thread exiting cleanly");
}
