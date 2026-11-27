//! Blocking work owned by a Lua host, including work whose awaiting coroutine is cancelled.

use std::sync::{Arc, Mutex};

use mlua::Lua;
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

#[derive(Clone, Default)]
pub struct Scope(Arc<Mutex<TaskTracker>>);

impl Scope {
    pub fn token(&self) -> mlua::Result<TaskTrackerToken> {
        let tracker = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if tracker.is_closed() {
            return Err(mlua::Error::external("host blocking work scope is closed"));
        }
        Ok(tracker.token())
    }

    pub(crate) fn spawn<R: Send + 'static>(
        &self,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> mlua::Result<JoinHandle<R>> {
        let token = self.token()?;
        Ok(tokio::task::spawn_blocking(move || {
            let _token = token;
            work()
        }))
    }

    pub fn close(&self) -> bool {
        let tracker = self.0.lock().unwrap_or_else(|error| error.into_inner());
        tracker.close();
        !tracker.is_empty()
    }

    pub async fn close_and_wait(&self) {
        let tracker = {
            let tracker = self.0.lock().unwrap_or_else(|error| error.into_inner());
            tracker.close();
            tracker.clone()
        };
        tracker.wait().await;
    }
}

pub fn scope(lua: &Lua) -> Scope {
    if let Some(scope) = lua.app_data_ref::<Scope>() {
        return scope.clone();
    }
    let scope = Scope::default();
    lua.set_app_data(scope.clone());
    scope
}

pub fn spawn<R: Send + 'static>(lua: &Lua, work: impl FnOnce() -> R + Send + 'static) -> mlua::Result<JoinHandle<R>> {
    scope(lua).spawn(work)
}

/// Stop new submissions and return whether any previously submitted blocking work is still running.
pub fn close(lua: &Lua) -> bool {
    scope(lua).close()
}

/// Stop new submissions, then wait for work already submitted by this host to finish.
/// Abort the host's async producers before calling this; blocking OS work cannot be aborted.
pub async fn close_and_wait(lua: &Lua) {
    scope(lua).close_and_wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn close_rejects_new_work_and_drains_cancelled_blocking_caller() {
        let scope = Scope::default();
        let (started, start) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = scope
            .spawn(move || {
                started.send(()).unwrap();
                released.recv().unwrap();
            })
            .unwrap();
        start.await.unwrap();
        worker.abort();
        let drain = scope.close_and_wait();
        tokio::pin!(drain);
        assert!(futures_util::poll!(drain.as_mut()).is_pending(), "cancelled caller must not detach blocking work");
        assert!(scope.spawn(|| ()).is_err(), "closed scope rejects new work");
        release.send(()).unwrap();
        drain.await;
        worker.await.unwrap();
    }
}
