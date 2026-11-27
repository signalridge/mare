//! Which runtime config write the current work belongs to. The host's config
//! writer runs a transaction inside this scope; an owner's `option/apply` callback, and every task
//! it spawns (through [`spawn_local`]), carry the transaction's number, so a config write issued
//! from inside it is refused at once instead of waiting on itself.
use std::future::Future;

tokio::task_local! {
    static TXN: u64;
    /// Identity of the connection/caller driving the current RPC or host work (never a security
    /// grant; used to bind pagination cursors to their originating caller under identical policy).
    static CALLER_OWNER: Option<String>;
}

/// Run `f` attributed to `owner` for caller-bound cursor/state checks.
pub async fn scope_caller_owner<F: Future>(owner: Option<String>, f: F) -> F::Output {
    CALLER_OWNER.scope(owner, f).await
}

/// The caller owner identity of the current scope, if any.
pub fn current_caller_owner() -> Option<String> {
    CALLER_OWNER.try_with(Clone::clone).ok().flatten()
}

/// Run `f` as part of transaction `id`.
pub async fn scope<F: Future>(id: u64, f: F) -> F::Output {
    TXN.scope(id, f).await
}

/// The transaction the current work belongs to, if any.
pub fn current() -> Option<u64> {
    TXN.try_with(|id| *id).ok()
}

/// Run a spawned future in the transaction scope captured where it was spawned.
pub async fn inherit<F: Future>(id: Option<u64>, f: F) -> F::Output {
    match id {
        Some(id) => TXN.scope(id, f).await,
        None => f.await,
    }
}

/// [`tokio::task::spawn_local`] carrying [`current`] and [`current_caller_owner`] across the task boundary.
pub fn spawn_local<F: Future + 'static>(future: F) -> tokio::task::JoinHandle<F::Output> {
    let owner = current_caller_owner();
    tokio::task::spawn_local(scope_caller_owner(owner, inherit(current(), future)))
}
