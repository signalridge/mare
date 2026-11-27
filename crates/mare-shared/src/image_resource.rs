//! Demand-loaded UI image bytes. This is a bounded materialization cache, never persistent
//! storage: the registered owner resolves opaque keys through its existing storage service.
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    static RESOURCE_SCOPE: u64;
}

thread_local! {
    static SYNC_RESOURCE_SCOPE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Active image-resolver namespace scope (`0` for the primary host, non-zero for isolated test hosts).
pub fn current_scope() -> u64 {
    if let Some(id) = SYNC_RESOURCE_SCOPE.with(std::cell::Cell::get) {
        return id;
    }
    RESOURCE_SCOPE.try_with(|id| *id).unwrap_or(0)
}

/// Run an async future under image-resolver namespace scope `scope_id`.
pub async fn in_scope<F: std::future::Future>(scope_id: u64, fut: F) -> F::Output {
    RESOURCE_SCOPE.scope(scope_id, fut).await
}

/// Run a synchronous closure under image-resolver namespace scope `scope_id`.
pub fn in_scope_sync<T>(scope_id: u64, f: impl FnOnce() -> T) -> T {
    struct Guard(Option<u64>);
    impl Drop for Guard {
        fn drop(&mut self) {
            SYNC_RESOURCE_SCOPE.with(|c| c.set(self.0));
        }
    }
    let prev = SYNC_RESOURCE_SCOPE.with(|c| c.replace(Some(scope_id)));
    let _guard = Guard(prev);
    f()
}

/// Portable reference carried by UI image nodes. Native resolver identities never cross
/// the wire; receiving frontends explicitly rebind this descriptor to a local resolver.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    #[schemars(length(min = 1, max = 64))]
    pub namespace: String,
    #[schemars(length(min = 1, max = 131072))]
    pub key: String,
    #[schemars(range(min = 1, max = 4096))]
    pub width: u32,
    #[schemars(range(min = 1, max = 4096))]
    pub height: u32,
}

pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const CACHE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PENDING: usize = 16;
pub const CACHE_ENTRIES: usize = 64;
static NEXT: AtomicU64 = AtomicU64::new(1);
static REGISTRY: OnceLock<Mutex<HashMap<u64, Weak<Resolver>>>> = OnceLock::new();
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
fn registry() -> &'static Mutex<HashMap<u64, Weak<Resolver>>> {
    REGISTRY.get_or_init(Default::default)
}
fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(Default::default)
}
type CacheKey = (u64, Arc<str>);
type CacheEntry = (CacheKey, Arc<[u8]>);
#[derive(Default)]
struct Cache {
    bytes: usize,
    entries: VecDeque<CacheEntry>,
}
impl Cache {
    fn get(&mut self, id: u64, key: &str) -> Option<Arc<[u8]>> {
        let index = self.entries.iter().position(|((scope, k), _)| *scope == id && k.as_ref() == key)?;
        let entry = self.entries.remove(index)?;
        let bytes = entry.1.clone();
        self.entries.push_back(entry);
        Some(bytes)
    }
    fn insert(&mut self, id: u64, key: Arc<str>, bytes: Arc<[u8]>) {
        if let Some(index) = self.entries.iter().position(|((scope, k), _)| *scope == id && k == &key)
            && let Some((_, old)) = self.entries.remove(index)
        {
            self.bytes -= old.len();
        }
        while self.bytes + bytes.len() > CACHE_BYTES || self.entries.len() >= CACHE_ENTRIES {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.len();
            } else {
                break;
            }
        }
        self.bytes += bytes.len();
        self.entries.push_back(((id, key), bytes));
    }
    fn remove_scope(&mut self, id: u64) {
        self.entries.retain(|((scope, _), _)| *scope != id);
        self.bytes = self.entries.iter().map(|(_, bytes)| bytes.len()).sum();
    }
}
/// Aggregate cache counters contain no keys or storage locators.
pub fn cache_info() -> (usize, usize) {
    let cache = cache().lock().unwrap();
    (cache.bytes, cache.entries.len())
}
/// One registration identity. Re-registration always gets a fresh identity, so cached bytes
/// cannot cross unload/reload or another owner's resolver, even with the same namespace/key.
pub struct Resolver {
    id: u64,
    sender: tokio::sync::mpsc::Sender<Request>,
    cancel: CancellationToken,
}
impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageResolver").field("id", &self.id).finish()
    }
}
impl Resolver {
    pub fn register() -> (Arc<Self>, tokio::sync::mpsc::Receiver<Request>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(MAX_PENDING);
        let resolver =
            Arc::new(Self { id: NEXT.fetch_add(1, Ordering::Relaxed), sender, cancel: CancellationToken::new() });
        registry().lock().unwrap().insert(resolver.id, Arc::downgrade(&resolver));
        (resolver, receiver)
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn close(&self) {
        self.cancel.cancel();
        registry().lock().unwrap().remove(&self.id);
        cache().lock().unwrap().remove_scope(self.id);
    }
    pub fn closed(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await
    }
}
impl Drop for Resolver {
    fn drop(&mut self) {
        self.close();
    }
}
/// Small immutable source retained by historical nodes. `key` includes the storage owner's
/// session/epoch and opaque locator; only the registered resolver interprets it.
#[derive(Clone, Debug)]
pub struct Reference {
    pub resolver: Arc<Resolver>,
    pub key: Arc<str>,
    pub width: u32,
    pub height: u32,
}
impl PartialEq for Reference {
    fn eq(&self, other: &Self) -> bool {
        self.resolver.id == other.resolver.id
            && self.key == other.key
            && self.width == other.width
            && self.height == other.height
    }
}
impl Eq for Reference {}
impl std::hash::Hash for Reference {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.resolver.id.hash(state);
        self.key.hash(state);
        self.width.hash(state);
        self.height.hash(state);
    }
}
impl Reference {
    pub fn new(id: u64, key: String, width: u32, height: u32) -> Result<Self, String> {
        if key.is_empty()
            || key.len() > 131072
            || width == 0
            || height == 0
            || width > 4096
            || height > 4096
            || u64::from(width) * u64::from(height) > 16777216
        {
            return Err("invalid image resource reference".into());
        }
        let resolver = registry()
            .lock()
            .unwrap()
            .get(&id)
            .and_then(Weak::upgrade)
            .filter(|r| !r.closed())
            .ok_or("image resolver is closed or foreign")?;
        Ok(Self { resolver, key: key.into(), width, height })
    }
    /// Called on an existing image worker, never the UI/draw thread. Both channel admission
    /// and byte size are bounded; closing the registration wakes/refuses outstanding work.
    pub fn load(&self) -> Result<Arc<[u8]>, String> {
        if self.resolver.closed() {
            return Err("image resolver closed".into());
        }
        if let Some(bytes) = cache().lock().unwrap().get(self.resolver.id, &self.key) {
            return Ok(bytes);
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancel = CancellationToken::new();
        let _cancel_on_return = cancel.clone().drop_guard();
        self.resolver
            .sender
            .try_send(Request { reference: self.clone(), sender: Some(sender), cancel })
            .map_err(|_| "image resolver busy or closed")?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.resolver.closed() {
                return Err("image resolver closed".into());
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(value) => return value,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("image resolver dropped request".into()),
                Err(_) if Instant::now() >= deadline => return Err("image resolver timeout".into()),
                Err(_) => {}
            }
        }
    }
}
pub struct Request {
    pub reference: Reference,
    sender: Option<mpsc::SyncSender<Result<Arc<[u8]>, String>>>,
    cancel: CancellationToken,
}
impl Request {
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await
    }
    pub fn complete(mut self, result: Result<Vec<u8>, String>) {
        let result = result.and_then(|bytes| {
            if self.reference.resolver.closed() || self.cancel.is_cancelled() {
                return Err("image resolver closed or request cancelled".into());
            }
            if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
                return Err("image resource exceeds byte limit".into());
            }
            let bytes: Arc<[u8]> = bytes.into();
            Ok(bytes)
        });
        if let Some(sender) = self.sender.take() {
            let cache_bytes = result.as_ref().ok().cloned();
            if sender.send(result).is_ok()
                && let Some(bytes) = cache_bytes
            {
                let mut cache = cache().lock().unwrap();
                if !self.reference.resolver.closed() {
                    cache.insert(self.reference.resolver.id, self.reference.key.clone(), bytes);
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_budget_evicts_and_registration_identity_isolates() {
        let mut c = Cache::default();
        for n in 0..8 {
            c.insert(1, n.to_string().into(), vec![n; MAX_IMAGE_BYTES].into());
        }
        assert_eq!(c.bytes, CACHE_BYTES);
        assert_eq!(c.entries.len(), 4);
        assert!(c.get(1, "0").is_none());
        assert!(c.get(2, "7").is_none());
        c.remove_scope(1);
        assert_eq!(c.bytes, 0);
    }
    #[test]
    fn close_releases_cached_bytes_and_refuses_old_identity() {
        let (r, _rx) = Resolver::register();
        let id = r.id();
        let reference = Reference::new(id, "owner/generation/opaque".into(), 32, 16).unwrap();
        r.close();
        assert!(reference.load().is_err());
        assert!(Reference::new(id, "x".into(), 32, 16).is_err());
        let (next, _rx) = Resolver::register();
        assert_ne!(id, next.id());
    }
}
