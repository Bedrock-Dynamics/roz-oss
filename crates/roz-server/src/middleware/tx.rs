//! Per-request transaction middleware with automatic RLS tenant context.
//!
//! The [`Tx`] extractor begins a Postgres transaction, calls
//! `roz_db::set_tenant_context` with the authenticated tenant, and hands
//! the transaction to the route handler via `FromRequestParts`.
//!
//! A response-layer middleware ([`tx_layer`]) resolves the transaction
//! after the handler returns:
//! - **2xx** responses commit the transaction.
//! - All other statuses roll back (the transaction is simply dropped).
//!
//! Handlers may also call [`Tx::commit`] explicitly to commit early.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use sqlx::{Postgres, Transaction};

use crate::state::AppState;

/// Shared slot between the [`Tx`] extractor and the commit layer.
///
/// When the handler drops `Tx` without calling [`Tx::commit`], `Drop`
/// puts the transaction back into this slot so the response layer can
/// decide whether to commit or rollback based on the HTTP status code.
pub type TxSlot = Arc<Mutex<Option<Transaction<'static, Postgres>>>>;

/// A per-request Postgres transaction with RLS tenant context already set.
///
/// Use `Deref`/`DerefMut` to access the inner `Transaction` for queries.
/// The transaction is committed automatically on 2xx responses by the
/// [`tx_layer`] response middleware, or call [`Tx::commit`] to
/// commit explicitly.
pub struct Tx {
    tx: Option<Transaction<'static, Postgres>>,
    slot: TxSlot,
}

impl Deref for Tx {
    type Target = Transaction<'static, Postgres>;

    fn deref(&self) -> &Self::Target {
        self.tx.as_ref().expect("Tx used after commit")
    }
}

impl DerefMut for Tx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.tx.as_mut().expect("Tx used after commit")
    }
}

impl Tx {
    /// Explicitly commit the transaction.
    ///
    /// Takes ownership so the `Drop` impl becomes a no-op (the inner
    /// `Option` is `None` after `.take()`).
    pub async fn commit(mut self) -> Result<(), sqlx::Error> {
        if let Some(tx) = self.tx.take() {
            tx.commit().await?;
        }
        Ok(())
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        // If the handler dropped Tx without committing, put the
        // transaction back into the shared slot so the response layer
        // can resolve it based on the HTTP status code.
        if let Some(tx) = self.tx.take() {
            *self.slot.lock() = Some(tx);
        }
    }
}

impl FromRequestParts<AppState> for Tx {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Read the AuthIdentity placed by the auth middleware.
        let auth = parts
            .extensions
            .get::<roz_core::auth::AuthIdentity>()
            .cloned()
            .ok_or_else(|| {
                (StatusCode::UNAUTHORIZED, "missing auth identity").into_response()
            })?;

        // Begin a transaction on the shared pool.
        let mut tx = state.pool.begin().await.map_err(|e| {
            tracing::error!(error = %e, "tx begin failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

        // Set the RLS tenant context so all queries within this
        // transaction are scoped to the authenticated tenant.
        roz_db::set_tenant_context(&mut *tx, auth.tenant_id().as_uuid())
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "set_tenant_context failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?;

        // Reuse the TxSlot pre-inserted by tx_layer, or create one.
        let slot = parts
            .extensions
            .get::<TxSlot>()
            .cloned()
            .unwrap_or_else(|| Arc::new(Mutex::new(None)));

        Ok(Self {
            tx: Some(tx),
            slot,
        })
    }
}

/// Axum middleware that manages the transaction lifecycle around handlers.
///
/// This middleware inserts a [`TxSlot`] into request extensions before
/// the handler runs. After the handler returns, it checks the slot:
///
/// - If the slot contains a transaction (handler dropped [`Tx`] without
///   calling [`Tx::commit`]):
///   - **2xx**: commit the transaction.
///   - **Other**: drop the transaction (implicit rollback).
/// - If the slot is empty (handler called [`Tx::commit`] explicitly, or
///   the handler did not use the [`Tx`] extractor): do nothing.
///
/// Apply as the innermost layer on authenticated routes:
/// ```ignore
/// let authenticated = Router::new()
///     .route(...)
///     .layer(axum::middleware::from_fn(tx_layer))
///     .layer(auth_middleware);
/// ```
pub async fn tx_layer(
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Pre-insert the slot so the Tx extractor can find it during
    // FromRequestParts (which runs inside next.run()).
    let slot: TxSlot = Arc::new(Mutex::new(None));
    req.extensions_mut().insert(slot.clone());

    let response = next.run(req).await;

    // Check if the handler's Tx put a transaction back into the slot
    // (handler dropped Tx without calling commit).
    let maybe_tx = slot.lock().take();

    if let Some(tx) = maybe_tx
        && response.status().is_success()
        && let Err(e) = tx.commit().await
    {
        tracing::error!(error = %e, "tx auto-commit failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    response
}
