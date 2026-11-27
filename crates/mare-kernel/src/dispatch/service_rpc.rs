//! JSON exports live on the existing service entry, never on a parallel RPC registry.
use std::collections::BTreeMap;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

use mlua::{Function, Lua, Table, Thread, Value};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;

use crate::{Kernel, ServiceContract};

fn validate_service_result(
    lua: &Lua,
    value: Value,
    response: &mare_shared::schema::Schema,
    result_schema: &Json,
    history_result: bool,
) -> Result<(Json, bool), String> {
    let node_limit = if history_result {
        mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES
    } else {
        mare_shared::lua::STRICT_JSON_MAX_NODES
    };
    let mut json = mare_shared::lua::strict_lua_to_json_bounded(lua, value, node_limit).map_err(|e| e.to_string())?;
    let reshaped = response.shape_and_validate_wrapped_result(&mut json, result_schema, node_limit)?;
    Ok((json, reshaped))
}

// A connection may hold registrations in several independent Host kernels, and release is
// routed to all of them. Per-kernel counters would let one release remove another Host's entry.
fn next_registration_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("registration id space exhausted");
    format!("registration-{id}")
}

#[derive(Default)]
struct ExportCallState {
    allow_provider_reload: Rc<Cell<bool>>,
}

struct CallCommitted {
    kernel_id: u64,
    provider_owner: u64,
    caller: u64,
    is_host_provider: bool,
    committed: Rc<Cell<bool>>,
}

tokio::task_local! {
    /// Per-call reload allowance for the JSON export running now (`Kernel::call_service_export`).
    static EXPORT_CALL: RefCell<ExportCallState>;
    /// Caller context id (`conn.ctx_id`) of the currently running JSON export.
    static EXPORT_CALLER: u64;
    /// Set once a service export crosses its durable commit point so client disconnect no longer
    /// aborts post-commit publication or cleanup. Scoped to the export provider's own owner so
    /// foreign plugins running in the same task (hooks, voters, downstream services) cannot
    /// prematurely flip the outer export call's commit flag.
    static CALL_COMMITTED: CallCommitted;
}

/// Caller context id of the currently running JSON export (`conn.ctx_id`), if any.
pub(crate) fn export_caller() -> Option<u64> {
    EXPORT_CALLER.try_with(|caller| *caller).ok()
}

/// Caller context id of the currently running JSON export (`conn.ctx_id`), if any.
pub fn current_export_caller() -> Option<u64> {
    export_caller()
}

/// Per-operation connection authorizer `(service, method, access) -> bool`.
pub type CallerAuthorizer = Rc<dyn Fn(&str, &str, &str) -> bool>;

/// Connection-level path-addressing authorizer (`Op::SessionLoadByPath`).
pub type CallerPathAuthorizer = Rc<dyn Fn() -> bool>;

/// Connection-level access profile attached to an RPC connection's kernel `ctx_id`.
#[derive(Clone)]
pub struct CallerAccess {
    pub is_peer: bool,
    pub allows: CallerAuthorizer,
    /// Authorizer for addressing sessions by host filesystem path (`Op::SessionLoadByPath`).
    pub allows_session_path: CallerPathAuthorizer,
}

/// Mark the running service export call as past its commit boundary when called by that export's
/// own service provider owner on the matching kernel.
pub fn mark_service_call_committed(kernel_id: u64) {
    let active = crate::active_owner_id();
    let _ = CALL_COMMITTED.try_with(|c| {
        if c.kernel_id == kernel_id && active == Some(c.provider_owner) {
            c.committed.set(true);
        }
    });
}

/// Whether the current task is directly executing a host-provided (`provider = "host"`) service
/// export call on `kernel_id` (as opposed to in-process Lua, a nested call from inside a plugin's
/// export, or an export belonging to a different kernel).
pub fn is_direct_host_export_call(kernel_id: u64) -> bool {
    let active = crate::active_owner_id();
    CALL_COMMITTED
        .try_with(|c| c.kernel_id == kernel_id && c.is_host_provider && (active.is_none() || active == Some(c.caller)))
        .unwrap_or(false)
}

/// Mark the running host-provided (`provider = "host"`) service export call on `kernel_id` as past
/// its commit boundary. Only host Rust implementations call this directly; foreign plugins cannot
/// bypass `mark_service_call_committed`'s owner check, nested foreign plugin owners cannot flip it,
/// and calls on a different kernel cannot collide with its numeric owner/caller ids.
pub fn mark_host_service_call_committed(kernel_id: u64) {
    let active = crate::active_owner_id();
    let _ = CALL_COMMITTED.try_with(|c| {
        if c.kernel_id == kernel_id && c.is_host_provider && (active.is_none() || active == Some(c.caller)) {
            c.committed.set(true);
        }
    });
}

/// RAII guard restoring the running export call's `allow_provider_reload` flag on drop.
pub struct ProviderReloadGuard(Option<(Rc<Cell<bool>>, bool)>);

impl Drop for ProviderReloadGuard {
    fn drop(&mut self) {
        if let Some((cell, prev)) = self.0.take() {
            cell.set(prev);
        }
    }
}

/// Mark the running export call as having initiated a full reload (`mare.reload()`), so
/// restarting the provider fiber as part of that reload does not abort the call with
/// `Unavailable`. Caller disposal and explicit request cancellation still abort it.
pub fn allow_provider_reload() -> ProviderReloadGuard {
    let inner = EXPORT_CALL
        .try_with(|slot| {
            let cell = slot.borrow().allow_provider_reload.clone();
            let prev = cell.replace(true);
            (cell, prev)
        })
        .ok();
    ProviderReloadGuard(inner)
}

pub use super::contracts::SERVICE_SCHEMA_MAX_BYTES;

#[derive(Clone)]
pub(crate) struct ServiceMethod {
    spec: std::sync::Arc<super::contracts::MethodSpec>,
    arguments: mare_shared::schema::Schema,
    response: mare_shared::schema::Schema,
    handler: Function,
    invoke: Function,
}

/// What the kernel derives from a provider's contract when it registers: JSON adapters and Lua
/// method guards are built lazily on first access for each method and cached for the lifetime of
/// the derived export entry.
pub(crate) struct ServiceExports {
    name: String,
    contract: std::sync::Arc<ServiceContract>,
    methods: RefCell<BTreeMap<String, ServiceMethod>>,
    guards: RefCell<BTreeMap<String, Table>>,
}

pub(crate) struct ProvideOptions {
    pub contract: Option<ServiceContract>,
    /// A backend property, not part of the contract: the implementation runs outside local
    /// confinement (a remote connection), so active sandboxes refuse to call it.
    pub unconfined: bool,
}

// Function::call_async terminates its suspended Rust future on cancellation. Keep the
// captured execution context bound through that termination and any Lua __close scopes,
// then restore the thread's previous binding (clearing a recycled thread, or preserving a
// caller's tagged owner thread when a tool body invokes an exported service).
type SavedCallBinding = Rc<RefCell<Option<(Thread, Option<Table>)>>>;

struct CallBinding {
    lua: Lua,
    binding: SavedCallBinding,
}
impl Drop for CallBinding {
    fn drop(&mut self) {
        if let Some((thread, previous)) = self.binding.borrow_mut().take() {
            let _ = mare_shared::exec_ctx::bind(&self.lua, &thread, previous);
        }
    }
}

pub use super::service_error::{ErrorValue, ServiceCallError, internal as internal_error};

fn invalid(message: impl ToString) -> mlua::Error {
    mlua::Error::runtime(message.to_string())
}

/// Call a registration's disposer and wait until the withdrawal is done (on the tree
/// the disposer returns a Future that settles once the effect's cleanup, asynchronous parts
/// included, has run). A plain function's return value is ignored.
pub async fn release_disposer(lua: &Lua, disposer: impl mlua::IntoLua) -> mlua::Result<()> {
    let settle = lua
        .load(
            r#"local d = ...
local r = d()
local ok, cordis = pcall(require, "cordis")
if ok and cordis.future.isThenable(r) then cordis.await(r) end"#,
        )
        .set_name("=release")
        .into_function()?;
    let disposer = disposer.into_lua(lua)?;
    mare_shared::lua::call_async::<()>(lua, &settle, disposer).await
}

pub(crate) fn parse_options(lua: &mlua::Lua, name: &str, options: Option<Table>) -> mlua::Result<ProvideOptions> {
    let Some(options) = options else { return Ok(ProvideOptions { contract: None, unconfined: false }) };
    for key in options.clone().pairs::<String, Value>() {
        if !matches!(key?.0.as_str(), "contract" | "unconfined") {
            return Err(invalid("unknown service provide option"));
        }
    }
    let unconfined = options.get::<Option<bool>>("unconfined")?.unwrap_or(false);
    let contract = match options.get::<Option<Table>>("contract")? {
        None => None,
        Some(contract) => {
            let wrapper = lua.create_table()?;
            wrapper.set(name, contract)?;
            let parsed = super::contracts::parse_contracts(lua, Value::Table(wrapper))?.remove(name);
            if parsed.as_ref().is_some_and(|c| c.essential) {
                return Err(invalid(format!("service `{name}` runtime contract cannot be essential")));
            }
            if parsed.as_ref().is_some_and(|c| c.methods.values().any(super::contracts::MethodSpec::is_host)) {
                return Err(invalid(format!("service `{name}` runtime contract cannot declare host-provided methods")));
            }
            parsed
        }
    };
    Ok(ProvideOptions { contract, unconfined })
}

/// How a JSON caller answers contract callbacks: its frontend sends `callback.call
/// {registration_id, callback, session_id?, turn_id?, tool_call_id?, arguments}` and returns the
/// result, optionally streaming partial updates via `callback.update
/// {id, partial}` into `update`. Dropping the future cancels the request (`$/cancel_request`).
pub trait CallbackChannel {
    fn call(
        &self,
        registration_id: String,
        callback: String,
        call_id: Option<String>,
        arguments: Json,
        update: Option<tokio::sync::mpsc::Sender<Json>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Json, ServiceCallError>>>>;
}

/// `(implementation, name, reader) -> receiver`: see `export_receiver`.
const EXPORT_RECEIVER: &str = r#"
local impl, name, reader = ...
if reader == nil then return impl end
return reader:get(name)
"#;

/// Compile a static Lua helper chunk once per VM and cache it in the named registry.
fn cached_lua_fn(lua: &Lua, key: &str, name: &str, source: &str) -> mlua::Result<Function> {
    if let Ok(func) = lua.named_registry_value::<Function>(key) {
        return Ok(func);
    }
    let func = lua.load(source).set_name(name).into_function()?;
    lua.set_named_registry_value(key, func.clone())?;
    Ok(func)
}

const SERVICE_INVOKE: &str =
    "local bind,handler,implementation,arguments=...;bind();return pcall(handler,implementation,arguments)";

impl ServiceExports {
    /// Prepare lazy export and guard derivation for a contract. The schemas were validated when
    /// the contract was parsed; nothing here reads the provider implementation or compiles
    /// per-method schemas/guards until a method is actually called or validated.
    pub(crate) fn derive(
        _lua: &Lua,
        name: &str,
        contract: &std::sync::Arc<ServiceContract>,
    ) -> mlua::Result<Option<Self>> {
        if contract.owner.is_some() {
            return Ok(None);
        }
        Ok(Some(Self {
            name: name.to_owned(),
            contract: std::sync::Arc::clone(contract),
            methods: RefCell::new(BTreeMap::new()),
            guards: RefCell::new(BTreeMap::new()),
        }))
    }

    /// Lazily compile and cache the JSON/RPC adapter for `method`.
    pub(crate) fn method(&self, lua: &Lua, method: &str) -> mlua::Result<Option<ServiceMethod>> {
        if let Some(hit) = self.methods.borrow().get(method).cloned() {
            return Ok(Some(hit));
        }
        let Some(spec) = self.contract.methods.get(method) else {
            return Ok(None);
        };
        let settle = cached_lua_fn(lua, "mare.service_settle", "=service-settle", super::contracts::SETTLE)?;
        let adapter_factory =
            cached_lua_fn(lua, "mare.service_adapter", "=service-adapter", super::contracts::ADAPTER)?;
        let invoke = cached_lua_fn(lua, "mare.service_invoke", "=service-invoke", SERVICE_INVOKE)?;
        let arguments = mare_shared::schema::Schema::compile_contract(spec.params.clone()).map_err(invalid)?;
        let response = mare_shared::schema::Schema::compile_contract(super::contracts::result_wrapper(&spec.result))
            .map_err(invalid)?;
        let order = lua.create_sequence_from(spec.order.iter().map(String::as_str))?;
        let handler: Function =
            adapter_factory.call((method, order, spec.rest, super::contracts::allows_null(&spec.result), settle))?;
        let built = ServiceMethod { spec: std::sync::Arc::new(spec.clone()), arguments, response, handler, invoke };
        let mut cache = self.methods.borrow_mut();
        Ok(Some(cache.entry(method.to_owned()).or_insert_with(|| built).clone()))
    }

    /// Lazily construct and cache the Lua proxy guard table for `method`.
    pub(crate) fn guard(&self, lua: &Lua, method: &str) -> mlua::Result<Option<Table>> {
        if let Some(hit) = self.guards.borrow().get(method).cloned() {
            return Ok(Some(hit));
        }
        let Some(spec) = self.contract.methods.get(method) else {
            return Ok(None);
        };
        let (spec_arc, schemas) = if spec.lua_unchecked {
            (std::sync::Arc::new(spec.clone()), None)
        } else if let Some(cached) = self.methods.borrow().get(method) {
            (std::sync::Arc::clone(&cached.spec), Some((cached.arguments.clone(), cached.response.clone())))
        } else {
            let arguments = mare_shared::schema::Schema::compile_contract(spec.params.clone()).map_err(invalid)?;
            let response =
                mare_shared::schema::Schema::compile_contract(super::contracts::result_wrapper(&spec.result))
                    .map_err(invalid)?;
            (std::sync::Arc::new(spec.clone()), Some((arguments, response)))
        };
        let failure = failure_metatable(lua)?;
        let built = guard(lua, &self.name, method, &self.contract, &spec_arc, schemas, &failure)?;
        let mut cache = self.guards.borrow_mut();
        Ok(Some(cache.entry(method.to_owned()).or_insert_with(|| built).clone()))
    }
}

/// The `mare.Error` value of `error`, for a Lua caller of the export surface (the tests' harness,
/// Kernel::call_export_from_lua).
#[cfg(feature = "test-support")]
pub(crate) fn error_value(lua: &Lua, error: &ServiceCallError) -> mlua::Result<Value> {
    super::service_error::lua_error(lua, &failure_metatable(lua)?, error)
}

/// Does the provider implement `method`? An optional contract method may be absent; it is then
/// neither listed nor callable. The set was read at registration exactly as validation reads
/// the provider (metatable `__index` included), so Lua and JSON callers see the same methods.
fn implemented(entry: &crate::ServiceEntry, method: &str) -> bool {
    entry.implemented.contains(method)
}

/// Errors a guard raises are values `{code, message, data?}` that print as
/// `code: message`.
pub(crate) fn failure_metatable(lua: &Lua) -> mlua::Result<Table> {
    mare_shared::lua_error::error_metatable(lua)
}

fn failure(lua: &Lua, meta: &Table, code: &str, message: String) -> mlua::Result<Value> {
    let error = lua.create_table()?;
    error.set("code", code)?;
    error.set(mare_shared::MESSAGE_KEY, message)?;
    error.set_metatable(Some(meta.clone()))?;
    Ok(Value::Table(error))
}

/// The Lua proxy's view of one contract method: `enter(...)` checks the arguments and
/// `leave(packed)` settles and checks the result. Both return an error value
/// instead of raising, so the proxy raises it as a table. `lua_unchecked` methods omit
/// `enter` and `leave` (`passthrough = true`) so they skip schema compilation and FFI checks.
fn guard(
    lua: &Lua,
    service: &str,
    method: &str,
    contract: &ServiceContract,
    spec: &std::sync::Arc<super::contracts::MethodSpec>,
    schemas: Option<(mare_shared::schema::Schema, mare_shared::schema::Schema)>,
    meta: &Table,
) -> mlua::Result<Table> {
    let guard = lua.create_table()?;
    // Registration methods must always run `leave` so disposer validation and callable-table
    // normalization apply even when `lua_unchecked = true` skips parameter/result JSON schemas.
    guard.set("passthrough", spec.lua_unchecked && !spec.registration)?;
    let unchecked_registration = spec.lua_unchecked && spec.registration;
    if unchecked_registration {
        guard.set("keep_extra", true)?;
    }
    let label = format!("{service}.{method}");
    let errors = Rc::new(super::contracts::method_errors(contract, method));
    {
        // the error a Lua caller catches for a raised value: core and declared
        // codes keep their data, anything else is a bounded `internal`
        let (service, method_name, lua_kind_errors, errors, meta) =
            (service.to_owned(), method.to_owned(), spec.lua_kind_errors, Rc::clone(&errors), meta.clone());
        guard.set(
            "fail",
            lua.create_function(move |lua, raised: Value| {
                // Lua-only methods that declare `lua_kind_errors = true` pass raised tables
                // carrying a string `kind` field straight through to in-process Lua callers
                // instead of settling them into `internal`.
                if lua_kind_errors
                    && let Value::Table(table) = &raised
                    && matches!(table.raw_get::<Value>("kind"), Ok(Value::String(_)))
                {
                    return Ok(raised);
                }
                let error = super::service_error::settle_error(lua, &service, &method_name, &errors, raised);
                super::service_error::lua_error(lua, &meta, &error)
            })?,
        )?;
    }
    let response_schema = if let Some((arguments, response)) = schemas {
        let (enter_spec, enter_meta, enter_label) = (std::sync::Arc::clone(spec), meta.clone(), label.clone());
        guard.set(
            "enter",
            lua.create_function(move |lua, args: mlua::MultiValue| {
                let given = args.clone();
                let checked = super::contracts::positional_object(lua, &enter_spec, args)
                    .map_err(|e| e.to_string())
                    .and_then(|mut object| {
                        // an empty table is the container the parameter schema declares, and
                        // the provider gets the tables so marked
                        let reshaped = arguments.shape_and_validate(&mut object)?;
                        if reshaped {
                            super::contracts::mark_arguments(lua, &enter_spec, &given, &object)
                                .map_err(|e| e.to_string())?;
                        }
                        Ok(())
                    });
                if let Err(error) = checked {
                    return failure(
                        lua,
                        &enter_meta,
                        "invalid_arguments",
                        format!("{enter_label}: invalid arguments: {error}"),
                    );
                }
                Ok(Value::Nil)
            })?,
        )?;
        Some(response)
    } else {
        None
    };
    if response_schema.is_some() || spec.registration {
        let (registration, history_result, meta) = (spec.registration, spec.history_result, meta.clone());
        let spec = std::sync::Arc::clone(spec);
        let nullable = !registration && super::contracts::allows_null(&spec.result);
        let (service, method_name) = (service.to_owned(), method.to_owned());
        guard.set(
            "leave",
            lua.create_function(move |lua, packed: Table| {
                // what SETTLE does for JSON callers, keeping a raised error value as it is
                let value = if packed.get::<i64>("n")? == 0 {
                    if nullable {
                        return Ok((Value::Nil, Value::Nil));
                    }
                    if registration { Value::Nil } else { Value::Table(lua.create_table()?) }
                } else {
                    match (packed.get::<Value>(1)?, packed.get::<Value>(2)?) {
                        (Value::Nil, Value::Nil) if nullable => {
                            return Ok((Value::Nil, Value::Nil));
                        }
                        (Value::Nil, Value::Nil) if registration => Value::Nil,
                        (Value::Nil, Value::Nil) => {
                            let error = ServiceCallError::coded("invalid_result", format!("{label}: returned nil"));
                            return Ok((Value::Nil, super::service_error::lua_error(lua, &meta, &error)?));
                        }
                        (Value::Nil, raised) if unchecked_registration && !raised.is_nil() => {
                            // Unchecked Lua registrations (e.g. `llm:register_provider`) may
                            // soft-fail by returning `(nil, err)` without raising; preserve both
                            // return values while still rejecting bare `nil` or non-disposers.
                            return Ok((Value::Nil, Value::Nil));
                        }
                        (Value::Nil, raised) => {
                            let error =
                                super::service_error::settle_error(lua, &service, &method_name, &errors, raised);
                            return Ok((Value::Nil, super::service_error::lua_error(lua, &meta, &error)?));
                        }
                        (value, _) => value,
                    }
                };
                if registration {
                    // a cordis EffectDisposer is a callable table: normalized here, once, to the
                    // function every later consumer (the Lua release, the JSON hold) expects. A Lua
                    // closure, so a cleanup that waits (I/O) can yield through it
                    let bind_id = match &value {
                        Value::Table(table) => table.get::<Option<Function>>("_bind_id").ok().flatten(),
                        _ => None,
                    };
                    let value = normalize_callable(lua, value)?;
                    if let (Some(bind_id), Value::Function(func)) = (bind_id, &value) {
                        store_bind_id(lua, func, bind_id)?;
                    }
                    return match value {
                        Value::Function(_) => Ok((value, Value::Nil)),
                        _ => Ok((
                            Value::Nil,
                            failure(
                                lua,
                                &meta,
                                "invalid_result",
                                format!("{label}: registration did not return a disposer"),
                            )?,
                        )),
                    };
                }
                let response = response_schema.as_ref().expect("non-registration leave has response schema");
                let checked = validate_service_result(lua, value.clone(), response, &spec.result, history_result)
                    .and_then(|(json, reshaped)| {
                        if reshaped {
                            mare_shared::lua::apply_shape(lua, &value, &json).map_err(|e| e.to_string())
                        } else {
                            Ok(())
                        }
                    });
                match checked {
                    Ok(()) => Ok((value, Value::Nil)),
                    Err(error) => Ok((
                        Value::Nil,
                        failure(lua, &meta, "invalid_result", format!("{label}: invalid result: {error}"))?,
                    )),
                }
            })?,
        )?;
    }
    Ok(guard)
}

impl Kernel {
    /// Remote code is never relabeled confined by a local wrapper or a JSON flag.
    pub fn check_service_scope(&self, unconfined: bool) -> Result<(), ServiceCallError> {
        if unconfined && mare_shared::exec_ctx::is_active(self.lua())? {
            return Err(ServiceCallError::Unconfined);
        }
        Ok(())
    }
    /// The caller `owner`'s lifecycle (an owner record of this kernel: a plugin's, the host's, an
    /// RPC connection's), never an id supplied in JSON. Calls are refused after the owner is disposed.
    pub fn service_caller(&self, owner: u64) -> Result<(u64, CancellationToken), ServiceCallError> {
        let state = self.st();
        let context =
            state.ctxs.get(&owner).filter(|context| !context.disposed).ok_or(ServiceCallError::CallerDisposed)?;
        Ok((owner, context.cancel.clone()))
    }
    /// Whether any live service currently exports a contract surface.
    pub fn has_service_exports(&self) -> bool {
        self.st().services.values().any(|entry| !entry.lifetime.is_cancelled() && entry.exports.is_some())
    }

    /// Fresh JSON descriptions of explicitly exported methods on the selected providers.
    /// This never executes provider code or infers an interface from implementation fields.
    pub fn service_exports(&self) -> Vec<Json> {
        // The catalog is the contract itself; the adapters were derived from it.
        let state = self.st();
        let mut services = BTreeMap::new();
        for (name, entry) in &state.services {
            if entry.lifetime.is_cancelled() {
                continue;
            }
            let Some(exports) = &entry.exports else { continue };
            let entry_name = name.clone();
            let methods: BTreeMap<_, _> = exports
                .contract
                .methods
                .iter()
                .filter(|(name, spec)| spec.remote() && implemented(entry, name))
                .map(|(name, spec)| {
                    let mut item = spec.describe();
                    item["unconfined"] = json!(entry.unconfined);
                    let errors: Vec<String> = super::contracts::method_errors(&entry.contract, name)
                        .keys()
                        .map(|error| mare_shared::errors::domain_code(&entry_name, error))
                        .collect();
                    if !errors.is_empty() {
                        item["errors"] = json!(errors);
                    }
                    (name.clone(), item)
                })
                .collect();
            let mut service = json!({"name":name,"methods":methods});
            if let Some(doc) = &entry.contract.doc {
                service["doc"] = json!(doc);
            }
            if !entry.contract.errors.is_empty() {
                service["errors"] = json!(super::contracts::declared_errors(name, &entry.contract));
            }
            if !entry.contract.capabilities.is_empty() {
                let caps: BTreeMap<_, _> = entry
                    .contract
                    .capabilities
                    .iter()
                    .filter(|(_, cap)| implemented(entry, &cap.method))
                    .map(|(cap_name, cap)| (cap_name.clone(), json!({ "method": cap.method, "doc": cap.doc })))
                    .collect();
                if !caps.is_empty() {
                    service["capabilities"] = json!(caps);
                }
            }
            services.insert(name.clone(), service);
        }
        services.into_values().collect()
    }

    /// Test support (feature `test-support`, enabled by mare-cli's dev-dependencies only; no
    /// product caller): a Lua caller of the JSON export surface, as `owner` — the arguments
    /// converted strictly (a Lua caller's empty tables take the shape the parameters declare),
    /// `options` = { cancel? }, and the outcome as `(true, value)` or `(false, mare.Error value)`
    /// (the caller raises it). Product callers use [`Kernel::call_service_export`].
    #[cfg(feature = "test-support")]
    pub async fn call_export_from_lua(
        &self,
        owner: u64,
        name: &str,
        method: &str,
        arguments: Value,
        options: Option<Table>,
    ) -> mlua::Result<(bool, Value)> {
        let lua = self.lua().clone();
        if let Some(options) = &options {
            for key in options.clone().pairs::<String, Value>() {
                if key?.0 != "cancel" {
                    let error = ServiceCallError::Arguments("unknown service call option".into());
                    return Ok((false, error_value(&lua, &error)?));
                }
            }
        }
        let cancel = match options
            .and_then(|options| options.get::<Option<mlua::AnyUserData>>("cancel").transpose())
            .transpose()?
        {
            Some(token) => token.borrow::<mare_shared::lua::Token>()?.0.clone(),
            None => CancellationToken::new(),
        };
        let arguments = if arguments.is_nil() {
            json!({})
        } else {
            match mare_shared::lua::strict_lua_to_json(&lua, arguments) {
                Ok(mut arguments) => {
                    if let Some(spec) = self.service_method(name, method) {
                        mare_shared::schema::shape_empty(&mut arguments, &spec.params);
                    }
                    arguments
                }
                Err(error) => {
                    let error = ServiceCallError::Arguments(error.to_string());
                    return Ok((false, error_value(&lua, &error)?));
                }
            }
        };
        match self.call_service_export(owner, None, name, method, arguments, cancel).await {
            Ok(value) => Ok((true, mare_shared::lua::json_to_lua_lossless(&lua, &value)?)),
            Err(error) => Ok((false, error_value(&lua, &error)?)),
        }
    }

    /// Invoke one JSON export with the same caller identity for Lua and RPC.
    /// Dropping/cancelling the call closes its Lua thread with the inherited execution context.
    /// `reader` is the caller's cordis ctx on the tree: a mare.Service provider is called on the
    /// service as `reader:get(name)` returns it, so `self.ctx` in the method is the caller.
    /// Without one (a Ctx userdata caller), a mare.Service method runs on the published instance,
    /// `self.ctx` being the provider.
    /// Validate export arguments and return requested callback names without invoking the provider.
    pub fn validate_export_call(
        &self,
        name: &str,
        method: &str,
        mut arguments: Json,
    ) -> Result<Vec<String>, ServiceCallError> {
        let exports = {
            let state = self.st();
            let entry = state.live_service(name).ok_or(ServiceCallError::Unavailable)?;
            let exports = entry.exports.as_ref().ok_or(ServiceCallError::MethodUnavailable)?;
            exports
                .contract
                .methods
                .get(method)
                .filter(|spec| spec.remote() && implemented(entry, method))
                .ok_or(ServiceCallError::MethodUnavailable)?;
            Rc::clone(exports)
        };
        let definition = exports.method(self.lua(), method)?.ok_or(ServiceCallError::MethodUnavailable)?;
        let wanted = take_callbacks(&mut arguments, &definition.spec)?;
        definition.arguments.validate(&arguments).map_err(ServiceCallError::Arguments)?;
        Ok(wanted)
    }

    pub async fn call_service_export(
        &self,
        caller: u64,
        reader: Option<Value>,
        name: &str,
        method: &str,
        arguments: Json,
        cancel: CancellationToken,
    ) -> Result<Json, ServiceCallError> {
        self.call_service_export_with_id(caller, reader, name, method, arguments, cancel, None).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn call_service_export_with_id(
        &self,
        caller: u64,
        reader: Option<Value>,
        name: &str,
        method: &str,
        arguments: Json,
        cancel: CancellationToken,
        registration_id: Option<String>,
    ) -> Result<Json, ServiceCallError> {
        if let Err(err) = mare_shared::session_barrier::check_unpoisoned(self.lua())
            && !self.allows_when_poisoned(name, method)
        {
            return Err(err.into());
        }
        let (_, caller_cancel) = self.service_caller(caller)?;
        let (implementation, exports, provider_owner, provider_cancel, unconfined, errors) = {
            let state = self.st();
            let entry = state.live_service(name).ok_or(ServiceCallError::Unavailable)?;
            let exports = entry.exports.as_ref().ok_or(ServiceCallError::MethodUnavailable)?;
            exports
                .contract
                .methods
                .get(method)
                .filter(|spec| spec.remote() && implemented(entry, method))
                .ok_or(ServiceCallError::MethodUnavailable)?;
            (
                entry.value.clone(),
                Rc::clone(exports),
                entry.ctx,
                entry.lifetime.clone(),
                entry.unconfined,
                super::contracts::method_errors(&entry.contract, method),
            )
        };
        let definition = exports.method(self.lua(), method)?.ok_or(ServiceCallError::MethodUnavailable)?;
        self.check_service_scope(unconfined)?;
        if !self.permitted_for_caller(Some(caller), name, method) {
            return Err(ServiceCallError::Denied);
        }
        let mut arguments = arguments;
        let wanted = take_callbacks(&mut arguments, &definition.spec)?;
        // checked as it was sent: a Lua caller (ctx:call_service) shaped its empty tables already
        definition.arguments.validate(&arguments).map_err(ServiceCallError::Arguments)?;
        let implementation = self.export_receiver(name, implementation, reader)?;
        let arguments = mare_shared::lua::json_to_lua_lossless(self.lua(), &arguments)
            .map_err(|e| ServiceCallError::Arguments(e.to_string()))?;
        let mut lifetime = None;
        // A call that does not end in a held registration cancels its callbacks' lifetime on exit.
        let mut lifetime_guard = CancelOnDrop(None);
        if !wanted.is_empty() {
            let channel = self.st().callback_channels.get(&caller).cloned().ok_or_else(|| {
                ServiceCallError::coded("unsupported", "this connection does not answer callback.call")
            })?;
            let id = registration_id.clone().unwrap_or_else(next_registration_id);
            // Decouple from `caller_cancel` once held so `dispose_ctx_inner` cancels this only
            // after `release_all` finishes running the registration's disposer.
            let token = CancellationToken::new();
            lifetime_guard.0 = Some(token.clone());
            if let Value::Table(object) = &arguments {
                for cb_name in &wanted {
                    let spec = definition.spec.remote_callbacks[cb_name].clone();
                    let function = self.remote_callback(&channel, &id, cb_name, spec, token.clone())?;
                    object.set(cb_name.as_str(), function)?;
                }
            }
            lifetime = Some((id, token));
        }
        let context = mare_shared::exec_ctx::current(self.lua());
        let binding = Rc::new(RefCell::new(None));
        let _binding = CallBinding { lua: self.lua().clone(), binding: binding.clone() };
        let bind = self.lua().create_function(move |lua, ()| {
            let current = lua.current_thread();
            let previous = mare_shared::exec_ctx::current(lua);
            mare_shared::exec_ctx::bind(lua, &current, context.clone())?;
            *binding.borrow_mut() = Some((current, previous));
            Ok(())
        })?;
        let reloading = Rc::new(Cell::new(false));
        let reloaded_provider = Rc::new(Cell::new(false));
        let committed = Rc::new(Cell::new(false));
        if cancel.is_cancelled() {
            return Err(ServiceCallError::Cancelled);
        }
        let is_host_provider = definition.spec.provider == super::contracts::MethodProvider::Host;
        let mut honor_cancel = true;
        let mut honor_caller_cancel = true;
        let mut honor_provider_cancel = true;
        let call_fut = EXPORT_CALLER.scope(
            caller,
            EXPORT_CALL.scope(
                RefCell::new(ExportCallState { allow_provider_reload: reloading.clone() }),
                CALL_COMMITTED.scope(
                    CallCommitted {
                        kernel_id: self.id(),
                        provider_owner,
                        caller,
                        is_host_provider,
                        committed: committed.clone(),
                    },
                    mare_shared::lua::call_async::<(bool, Value)>(
                        self.lua(),
                        &definition.invoke,
                        (bind, definition.handler, implementation, arguments),
                    ),
                ),
            ),
        );
        tokio::pin!(call_fut);
        let result = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if honor_cancel => {
                    if committed.get() {
                        honor_cancel = false;
                    } else {
                        return Err(ServiceCallError::Cancelled);
                    }
                }
                _ = caller_cancel.cancelled(), if honor_caller_cancel => {
                    if committed.get() {
                        honor_caller_cancel = false;
                    } else {
                        return Err(ServiceCallError::CallerDisposed);
                    }
                }
                _ = async {
                    provider_cancel.cancelled().await;
                    if reloading.get() {
                        reloaded_provider.set(true);
                        std::future::pending::<()>().await;
                    }
                }, if honor_provider_cancel => {
                    if is_host_provider && committed.get() {
                        honor_provider_cancel = false;
                    } else {
                        return Err(ServiceCallError::Unavailable);
                    }
                }
                result = &mut call_fut => break result?,
            }
        };
        let result = match result {
            (true, value) => value,
            (false, raised) => {
                return Err(super::service_error::settle_error(self.lua(), name, method, &errors, raised));
            }
        };
        let caller_committed = committed.get() && !definition.spec.registration;
        let host_committed = is_host_provider && committed.get();
        let cancelled = if cancel.is_cancelled() && !committed.get() {
            Some(ServiceCallError::Cancelled)
        } else if caller_cancel.is_cancelled() && !caller_committed {
            Some(ServiceCallError::CallerDisposed)
        } else if provider_cancel.is_cancelled() && !reloading.get() && !reloaded_provider.get() && !host_committed {
            Some(ServiceCallError::Unavailable)
        } else {
            None
        };
        if let Some(err) = cancelled {
            self.release_undelivered_registration(name, definition.spec.registration, result).await;
            return Err(err);
        }
        let result = if definition.spec.registration {
            let reg_id = lifetime.as_ref().map(|(id, _)| id.clone()).or(registration_id);
            let held = self.hold_registration(caller, name, result, reg_id).await?;
            lifetime_guard.0 = None;
            if let Some((id, token)) = &lifetime {
                self.st().callback_lifetimes.insert(id.clone(), token.clone());
            }
            held
        } else {
            let mut json = mare_shared::lua::strict_lua_to_json(self.lua(), result)
                .map_err(|error| ServiceCallError::Result(error.to_string()))?;
            mare_shared::schema::shape_empty(&mut json, &definition.spec.result);
            json
        };
        definition.response.validate(&json!({"value": result})).map_err(ServiceCallError::Result)?;
        Ok(result)
    }

    /// The receiver of an export call: the mare.Service read through `reader` (traced, `self.ctx` =
    /// the caller), or the provider itself for a caller without a ctx.
    fn export_receiver(
        &self,
        name: &str,
        implementation: Value,
        reader: Option<Value>,
    ) -> Result<Value, ServiceCallError> {
        // compiled once per VM
        const KEY: &str = "mare.service_export_receiver";
        let lua = self.lua();
        let receiver: Function = match lua.named_registry_value::<Function>(KEY) {
            Ok(f) => f,
            Err(_) => {
                let f = lua.load(EXPORT_RECEIVER).set_name("=service-export-receiver").into_function()?;
                lua.set_named_registry_value(KEY, f.clone())?;
                f
            }
        };
        match receiver.call::<Value>((implementation, name, reader))? {
            // the provider is no longer visible from the caller's ctx
            Value::Nil => Err(ServiceCallError::Unavailable),
            traced => Ok(traced),
        }
    }

    /// The contract entry of a method the selected provider exports to JSON callers.
    pub fn service_method(&self, name: &str, method: &str) -> Option<super::contracts::MethodSpec> {
        let state = self.st();
        let entry = state.live_service(name)?;
        entry
            .exports
            .as_ref()?
            .contract
            .methods
            .get(method)
            .filter(|spec| spec.remote() && implemented(entry, method))
            .cloned()
    }

    /// Whether the active provider for `service` declares and implements the named contract `capability`.
    pub fn service_has_capability(&self, service: &str, capability: &str) -> bool {
        let state = self.st();
        let Some(entry) = state.live_service(service) else { return false };
        let Some(cap) = entry.contract.capabilities.get(capability) else { return false };
        implemented(entry, &cap.method)
    }

    /// Return the first capability required by `service.method` that the active provider does not
    /// implement, or `None` when all required capabilities are satisfied (or the method requires none).
    pub fn method_missing_capability(&self, service: &str, method: &str) -> Option<String> {
        let state = self.st();
        let entry = state.live_service(service)?;
        let spec = entry.contract.methods.get(method)?;
        for cap_name in &spec.requires_capabilities {
            let ok = entry.contract.capabilities.get(cap_name).is_some_and(|cap| implemented(entry, &cap.method));
            if !ok {
                return Some(cap_name.clone());
            }
        }
        None
    }

    /// Register or clear the connection-level [`CallerAccess`] policy for `ctx_id`.
    pub fn set_caller_access(&self, ctx: u64, access: Option<CallerAccess>) {
        let mut st = self.st();
        match access {
            Some(access) => {
                st.caller_access.insert(ctx, access);
            }
            None => {
                st.caller_access.remove(&ctx);
            }
        }
    }

    /// Whether `caller` is a registered restricted peer connection `ctx_id`.
    pub fn is_peer_caller(&self, caller: Option<u64>) -> bool {
        caller.and_then(|id| self.st().caller_access.get(&id).map(|access| access.is_peer)).unwrap_or(false)
    }

    /// Whether `caller` is permitted to address sessions by filesystem path (unified with
    /// `Op::SessionLoadByPath`: local/admin owners only; never peers or non-admin remote owners).
    pub fn allows_session_path(&self, caller: Option<u64>) -> bool {
        let Some(caller_id) = caller else {
            return true;
        };
        let access = self.st().caller_access.get(&caller_id).cloned();
        match access {
            None => true,
            Some(access) => (access.allows_session_path)(),
        }
    }

    /// May `caller` (an RPC connection `ctx_id`, or `None` for unrestricted in-process work)
    /// access `name.method`? Services without a full contract carry no access rules and are allowed.
    pub fn permitted_for_caller(&self, caller: Option<u64>, name: &str, method: &str) -> bool {
        let Some(caller_id) = caller else {
            return true;
        };
        let (access, remote, spec_access) = {
            let state = self.st();
            let Some(access) = state.caller_access.get(&caller_id).cloned() else {
                return true;
            };
            let (remote, spec_access) = match state.live_service(name) {
                Some(entry) => match entry.contract.methods.get(method) {
                    Some(spec) => (spec.remote(), spec.access_name().to_owned()),
                    None => return false,
                },
                None => return true,
            };
            (access, remote, spec_access)
        };
        !remote || (access.allows)(name, method, &spec_access)
    }

    /// Check `value` against the result contract of `name.method` without calling anything, so
    /// a provider can validate a result before it commits a change.
    pub fn validate_result(&self, name: &str, method: &str, value: Value) -> Result<(), String> {
        let exports = {
            let state = self.st();
            let entry =
                state.live_service(name).ok_or_else(|| format!("unavailable: service `{name}` has no provider"))?;
            let exports = entry
                .exports
                .as_ref()
                .filter(|exports| exports.contract.methods.contains_key(method))
                .ok_or_else(|| format!("method_not_found: `{name}.{method}` declares no result"))?;
            Rc::clone(exports)
        };
        let definition = exports
            .method(self.lua(), method)
            .map_err(|e| format!("invalid_result: {name}.{method}: {e}"))?
            .ok_or_else(|| format!("method_not_found: `{name}.{method}` declares no result"))?;
        validate_service_result(
            self.lua(),
            value,
            &definition.response,
            &definition.spec.result,
            definition.spec.history_result,
        )
        .map(|_| ())
        .map_err(|e| format!("invalid_result: {name}.{method}: {e}"))
    }

    async fn release_undelivered_registration(&self, plugin: &str, registration: bool, value: Value) {
        if !registration {
            return;
        }
        match normalize_callable(self.lua(), value) {
            Ok(Value::Function(dispose)) => {
                if let Err(error) = release_disposer(self.lua(), dispose).await {
                    self.notify_error(plugin, "registration release", &error.to_string());
                }
            }
            Ok(_) => {}
            Err(error) => self.notify_error(plugin, "registration release", &error.to_string()),
        }
    }

    /// A JSON caller keeps a Lua disposer by id. It belongs to the caller's ctx: the kernel
    /// calls every disposer it still holds when that ctx is disposed (connection close). If the
    /// caller already disappeared before insertion, the disposer is released immediately.
    async fn hold_registration(
        &self,
        caller: u64,
        plugin: &str,
        value: Value,
        id: Option<String>,
    ) -> Result<Json, ServiceCallError> {
        let mut bind_id: Option<Function> = match &value {
            Value::Table(table) => table.get("_bind_id").ok().flatten(),
            _ => None,
        };
        let value = normalize_callable(self.lua(), value).map_err(|e| ServiceCallError::Result(e.to_string()))?;
        let Value::Function(dispose) = value else {
            return Err(ServiceCallError::Result("registration did not return a disposer".into()));
        };
        if bind_id.is_none() {
            bind_id = take_bind_id(self.lua(), &dispose).ok().flatten();
        }
        match self.insert_registration(caller, id, dispose.clone()) {
            Ok(id) => {
                if let Some(bind) = bind_id
                    && let Err(bind_err) = bind.call_async::<()>(id.clone()).await
                {
                    let _ = self.release_registration(caller, &id).await;
                    return Err(ServiceCallError::Result(bind_err.to_string()));
                }
                Ok(json!({ "registration_id": id }))
            }
            Err(err) => {
                if let Err(release_err) = release_disposer(self.lua(), dispose).await {
                    self.notify_error(plugin, "registration release", &release_err.to_string());
                }
                Err(err)
            }
        }
    }

    /// Register a connection-owned disposer in the kernel's unified registration table so
    /// `registrations.release { registration_id }` and `dispose_ctx` release it uniformly.
    pub fn insert_connection_registration(
        &self,
        caller: u64,
        id: Option<String>,
        dispose: Value,
        cancel: Option<CancellationToken>,
    ) -> Result<String, ServiceCallError> {
        let value = normalize_callable(self.lua(), dispose).map_err(|e| ServiceCallError::Result(e.to_string()))?;
        let Value::Function(dispose) = value else {
            return Err(ServiceCallError::Result("registration did not return a disposer".into()));
        };
        let id = self.insert_registration(caller, id, dispose)?;
        if let Some(token) = cancel {
            self.st().callback_lifetimes.insert(id.clone(), token);
        }
        Ok(id)
    }

    /// IDs of registrations currently held for `caller`'s owner matching `prefix`.
    pub fn owner_registrations_matching(&self, caller: u64, prefix: &str) -> Vec<String> {
        let Ok((owner, _)) = self.service_caller(caller) else {
            return Vec::new();
        };
        self.st()
            .ctxs
            .get(&owner)
            .map(|ctx| ctx.registrations.keys().filter(|k| k.starts_with(prefix)).cloned().collect())
            .unwrap_or_default()
    }

    fn insert_registration(
        &self,
        caller: u64,
        id: Option<String>,
        dispose: Function,
    ) -> Result<String, ServiceCallError> {
        let mut state = self.st();
        if !state.ctxs.get(&caller).is_some_and(|context| !context.disposed) {
            return Err(ServiceCallError::CallerDisposed);
        }
        let id = id.unwrap_or_else(next_registration_id);
        let context = state.ctxs.get_mut(&caller).expect("checked non-disposed caller above");
        context.registrations.insert(id.clone(), std::rc::Rc::new(tokio::sync::Mutex::new(Some(dispose))));
        Ok(id)
    }

    /// Release one registration held for `caller`. Unknown, foreign or already released ids
    /// return false. Releases of one registration are serialized (with each other and with the
    /// owner's disposal); the disposer is dropped only after it succeeded, so a failed release
    /// can be retried and a concurrent one never runs it twice.
    pub async fn release_registration(&self, caller: u64, id: &str) -> Result<bool, ServiceCallError> {
        let (owner, _) = self.service_caller(caller)?;
        let handle = self.st().ctxs.get(&owner).and_then(|context| context.registrations.get(id).cloned());
        let Some(handle) = handle else { return Ok(false) };
        self.end_callbacks(id);
        let mut held = handle.lock().await;
        let Some(dispose) = held.clone() else { return Ok(false) };
        release_disposer(self.lua(), dispose).await?;
        *held = None;
        drop(held);
        if let Some(context) = self.st().ctxs.get_mut(&owner) {
            context.registrations.remove(id);
        }
        Ok(true)
    }

    /// Release every registration a disposed ctx still holds, each under its lock. A disposer
    /// that fails keeps its handle in the kernel's pending list, retried by
    /// [`Kernel::retry_releases`] (each later disposal, and shutdown).
    /// Release `held` for `plugin`. Every handle is queued first and leaves the queue only once
    /// released, so a release cancelled midway (a shutdown deadline) loses none: the next
    /// retry_releases picks them up.
    pub(crate) async fn release_all(&self, plugin: &str, held: Vec<crate::Registration>) {
        self.st().pending_releases.extend(held.into_iter().map(|handle| (plugin.to_owned(), handle)));
        self.retry_releases().await;
    }

    async fn release_held(&self, handle: &crate::Registration) -> mlua::Result<()> {
        let mut held = handle.lock().await;
        if let Some(dispose) = held.clone() {
            release_disposer(self.lua(), dispose).await?;
            *held = None;
        }
        Ok(())
    }

    /// Retry the releases that failed when their owners were disposed; returns how many are
    /// still pending.
    pub async fn retry_releases(&self) -> usize {
        // a snapshot to walk; each handle stays queued until its release succeeds
        let pending: Vec<(String, crate::Registration)> = self.st().pending_releases.clone();
        for (plugin, handle) in pending {
            match self.release_held(&handle).await {
                Ok(()) => self.st().pending_releases.retain(|(_, queued)| !std::rc::Rc::ptr_eq(queued, &handle)),
                Err(error) => self.notify_error(&plugin, "registration release", &error.to_string()),
            }
        }
        self.st().pending_releases.len()
    }
}

fn normalize_callable(lua: &Lua, value: Value) -> mlua::Result<Value> {
    match value {
        Value::Table(table) if table.metatable().is_some_and(|m| m.contains_key("__call").unwrap_or(false)) => {
            Ok(Value::Function(forward_callable(lua)?.call::<Function>(table)?))
        }
        other => Ok(other),
    }
}

/// `forward(callable) -> function(...) return callable(...) end`, compiled once per Lua state: a
/// Lua closure (not a Rust one), so the call it forwards may yield.
fn forward_callable(lua: &Lua) -> mlua::Result<Function> {
    const KEY: &str = "mare.forward_callable";
    if let Ok(f) = lua.named_registry_value::<Function>(KEY) {
        return Ok(f);
    }
    let f: Function = lua
        .load("return function(callable) return function(...) return callable(...) end end")
        .set_name("=forward_callable")
        .eval()?;
    lua.set_named_registry_value(KEY, &f)?;
    Ok(f)
}

fn bind_id_table(lua: &Lua) -> mlua::Result<Table> {
    const KEY: &str = "mare.registration_bind_ids";
    if let Ok(t) = lua.named_registry_value::<Table>(KEY) {
        return Ok(t);
    }
    let t = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__mode", "k")?;
    t.set_metatable(Some(mt))?;
    lua.set_named_registry_value(KEY, &t)?;
    Ok(t)
}

fn store_bind_id(lua: &Lua, dispose: &Function, bind_id: Function) -> mlua::Result<()> {
    bind_id_table(lua)?.set(dispose.clone(), bind_id)
}

fn take_bind_id(lua: &Lua, dispose: &Function) -> mlua::Result<Option<Function>> {
    let t = bind_id_table(lua)?;
    let bind: Option<Function> = t.get(dispose.clone())?;
    t.set(dispose.clone(), Value::Nil)?;
    Ok(bind)
}

/// Cancels a callbacks lifetime unless forgotten when the registration is held.
struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.cancel();
        }
    }
}

/// The callbacks a JSON caller says it implements (`callbacks: [...]`), removed from the
/// arguments: only declared ones, each once, every required one present.
fn take_callbacks(arguments: &mut Json, spec: &super::contracts::MethodSpec) -> Result<Vec<String>, ServiceCallError> {
    if spec.remote_callbacks.is_empty() {
        return Ok(vec![]);
    }
    let listed = arguments.as_object_mut().and_then(|object| object.remove("callbacks"));
    let mut wanted: Vec<String> = vec![];
    match listed {
        None | Some(Json::Null) => {}
        Some(Json::Array(names)) => {
            for name in names {
                let Json::String(name) = name else {
                    return Err(ServiceCallError::Arguments("callbacks lists callback names".into()));
                };
                if !spec.remote_callbacks.contains_key(&name) || wanted.contains(&name) {
                    return Err(ServiceCallError::Arguments(format!(
                        "`{name}` is not a callback of this method, or listed twice"
                    )));
                }
                wanted.push(name);
            }
        }
        Some(_) => return Err(ServiceCallError::Arguments("callbacks lists callback names".into())),
    }
    if let Some((name, _)) = spec.remote_callbacks.iter().find(|(name, cb)| !cb.optional && !wanted.contains(name)) {
        return Err(ServiceCallError::Arguments(format!("callback `{name}` is required")));
    }
    Ok(wanted)
}

impl Kernel {
    /// Set (or clear) how `ctx` answers contract callbacks (`callback.call`); cleared with the ctx.
    pub fn set_callback_channel(&self, ctx: u64, channel: Option<Rc<dyn CallbackChannel>>) {
        let mut state = self.st();
        match channel {
            Some(channel) => {
                state.callback_channels.insert(ctx, channel);
            }
            None => {
                state.callback_channels.remove(&ctx);
            }
        }
    }

    /// The registration is gone: its callbacks stop, and calls in flight are cancelled.
    pub(crate) fn end_callbacks(&self, id: &str) {
        if let Some(token) = self.st().callback_lifetimes.remove(id) {
            token.cancel();
        }
    }

    /// Build the Lua function representing one JSON-implemented callback: positional arguments map
    /// via `order` and are checked against `params`, `callback.call` waits cleanly across yield,
    /// incremental `callback.update` partials stream to an optional trailing `on_update` callback
    /// when `update` schema is declared, and the client's reply is checked against `result`.
    fn remote_callback(
        &self,
        channel: &Rc<dyn CallbackChannel>,
        id: &str,
        name: &str,
        spec: super::contracts::CallbackSpec,
        lifetime: CancellationToken,
    ) -> mlua::Result<Function> {
        const UPDATE_CHANNEL_CAPACITY: usize = 1024;
        struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                if let Some(handle) = self.0.take() {
                    handle.abort();
                }
            }
        }

        let label = format!("callback `{name}`");
        let arguments =
            mare_shared::schema::Schema::compile_contract(spec.params.clone()).map_err(mlua::Error::runtime)?;
        let response = mare_shared::schema::Schema::compile_contract(super::contracts::result_wrapper(&spec.result))
            .map_err(mlua::Error::runtime)?;
        let update_schema = spec
            .update
            .as_ref()
            .map(|schema| mare_shared::schema::Schema::compile_contract(super::contracts::result_wrapper(schema)))
            .transpose()
            .map_err(mlua::Error::runtime)?
            .map(Rc::new);
        let (channel, id, name) = (channel.clone(), id.to_owned(), name.to_owned());
        let meta = failure_metatable(self.lua())?;
        let spec = Rc::new(spec);
        let (arguments, response) = (Rc::new(arguments), Rc::new(response));
        self.lua()
            .create_async_function(move |lua, args: mlua::MultiValue| {
                let (channel, id, name, meta, label) =
                    (channel.clone(), id.clone(), name.clone(), meta.clone(), label.clone());
                let (spec, arguments, response, update_schema, lifetime) =
                    (spec.clone(), arguments.clone(), response.clone(), update_schema.clone(), lifetime.clone());
                let call_id = mare_shared::exec_ctx::current(&lua)
                    .and_then(|t| t.get::<Option<String>>("tool_call_id").ok().flatten());
                async move {
                    let raise = |error: ServiceCallError| -> mlua::Result<(bool, Value)> {
                        Ok((false, super::service_error::lua_error(&lua, &meta, &error)?))
                    };
                    if lifetime.is_cancelled() {
                        return raise(ServiceCallError::coded(
                            "cancelled",
                            format!("{label} ended with its registration"),
                        ));
                    }
                    let mut raw_args: Vec<Value> = args.into_iter().collect();
                    let mut on_update: Option<Function> = None;
                    let mut call_cancel: Option<CancellationToken> = None;
                    while raw_args.len() > spec.order.len() {
                        match raw_args.last() {
                            Some(Value::Nil) => {
                                raw_args.pop();
                            }
                            Some(Value::Function(_)) if on_update.is_none() => {
                                if let Some(Value::Function(f)) = raw_args.pop() {
                                    on_update = Some(f);
                                }
                            }
                            Some(Value::UserData(ud)) if call_cancel.is_none() => {
                                if let Ok(tok) = ud.borrow::<mare_shared::lua::Token>() {
                                    call_cancel = Some(tok.0.clone());
                                    raw_args.pop();
                                } else {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }
                    if let Some(cancel) = &call_cancel
                        && cancel.is_cancelled()
                    {
                        return raise(ServiceCallError::Cancelled);
                    }
                    if on_update.is_some() && update_schema.is_none() {
                        return raise(ServiceCallError::Arguments(format!(
                            "{label}: update callback passed, but contract does not declare `update`"
                        )));
                    }
                    let mut params = match super::contracts::positional_fields(
                        &lua,
                        &spec.order,
                        false,
                        &std::collections::BTreeMap::new(),
                        &std::collections::BTreeSet::new(),
                        mlua::MultiValue::from_vec(raw_args),
                    ) {
                        Ok(params) => params,
                        Err(err) => return raise(ServiceCallError::Arguments(format!("{label}: {err}"))),
                    };
                    mare_shared::schema::shape_empty(&mut params, &spec.params);
                    if let Err(error) = arguments.validate(&params) {
                        return raise(ServiceCallError::Arguments(format!("{label}: {error}")));
                    }
                    let (update_tx, mut worker) = if let (Some(on_update), Some(schema)) = (on_update, update_schema)
                    {
                        let (tx, mut rx) = tokio::sync::mpsc::channel::<Json>(UPDATE_CHANNEL_CAPACITY);
                        let worker_lua = lua.clone();
                        let worker_label = label.clone();
                        let handle = tokio::task::spawn_local(async move {
                            while let Some(partial) = rx.recv().await {
                                if let Err(error) = schema.validate(&json!({"value": partial})) {
                                    tracing::debug!("{worker_label} ignoring invalid callback.update partial: {error}");
                                    continue;
                                }
                                match mare_shared::lua::json_to_lua_lossless(&worker_lua, &partial) {
                                    Ok(lua_partial) => {
                                        if let Err(err) = on_update.call_async::<()>(lua_partial).await {
                                            tracing::debug!("{worker_label} update callback failed: {err}");
                                        }
                                    }
                                    Err(err) => {
                                        tracing::debug!("{worker_label} invalid callback.update payload: {err}");
                                    }
                                }
                            }
                        });
                        (Some(tx), AbortOnDrop(Some(handle)))
                    } else {
                        (None, AbortOnDrop(None))
                    };
                    async fn cancelled_or_pending(cancel: Option<&CancellationToken>) {
                        match cancel {
                            Some(token) => token.cancelled().await,
                            None => std::future::pending().await,
                        }
                    }
                    let reply = tokio::select! {
                        biased;
                        _ = lifetime.cancelled() => {
                            return raise(ServiceCallError::coded("cancelled", format!("{label} ended with its registration")));
                        }
                        _ = cancelled_or_pending(call_cancel.as_ref()) => {
                            return raise(ServiceCallError::Cancelled);
                        }
                        reply = channel.call(id, name, call_id, params, update_tx) => reply,
                    };
                    if let Some(mut handle) = worker.0.take() {
                        tokio::select! {
                            biased;
                            _ = lifetime.cancelled() => {
                                handle.abort();
                                return raise(ServiceCallError::coded("cancelled", format!("{label} ended with its registration")));
                            }
                            _ = cancelled_or_pending(call_cancel.as_ref()) => {
                                handle.abort();
                                return raise(ServiceCallError::Cancelled);
                            }
                            _ = &mut handle => {}
                        }
                    }
                    let value = match reply {
                        Ok(value) => value,
                        Err(ServiceCallError::Coded(ev)) if !mare_shared::errors::is_core(&ev.code) => {
                            return raise(super::service_error::internal(
                                &label,
                                &format!("{}: {}", ev.code, ev.message),
                            ));
                        }
                        Err(error) => return raise(error),
                    };
                    if let Err(error) = response.validate(&json!({"value": value})) {
                        return raise(ServiceCallError::Result(format!("{label}: {error}")));
                    }
                    match mare_shared::lua::json_to_lua_lossless(&lua, &value) {
                        Ok(val) => Ok((true, val)),
                        Err(error) => raise(ServiceCallError::Result(format!("{label}: {error}"))),
                    }
                }
            })
            .and_then(|call| {
                self.lua()
                    .load("local call = ...; return function(...) local ok, v = call(...) if not ok then error(v, 2) end return v end")
                    .set_name("=callback")
                    .call::<Function>(call)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hold_registration_releases_callable_table_and_reports_failing_disposer_for_disposed_owner() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let owner = k.open_owner("gone-caller", "user").unwrap();
                k.dispose_ctx(owner).await;

                // Missing/disposed owner runs the callable-table disposer immediately instead of keeping it.
                let disposer: Value = k
                    .lua()
                    .load("RELEASED = 0; return setmetatable({}, { __call = function() RELEASED = RELEASED + 1 end })")
                    .eval()
                    .unwrap();
                let err = k.hold_registration(owner, "svc", disposer, None).await.unwrap_err();
                assert!(matches!(err, ServiceCallError::CallerDisposed), "{err}");
                assert_eq!(k.lua().load("return RELEASED").eval::<i64>().unwrap(), 1);

                // Disposer failure on undelivered cleanup is reported via notify_error, not swallowed.
                let failing: Value = k.lua().load("return function() error('UNDELIVERED-BOOM') end").eval().unwrap();
                let err = k.hold_registration(owner, "svc", failing, None).await.unwrap_err();
                assert!(matches!(err, ServiceCallError::CallerDisposed), "{err}");
                let problems = k.problems().join("\n");
                assert!(problems.contains("UNDELIVERED-BOOM"), "{problems}");
            })
            .await;
    }

    #[tokio::test]
    async fn caller_and_provider_cancel_during_registration_export_release_disposer() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let caller = k.open_owner("caller", "user").unwrap();
                let caller_cancel = k.st().ctxs.get(&caller).unwrap().cancel.clone();
                let provider = k.open_owner("provider", "user").unwrap();
                let cancel_fn = k
                    .lua()
                    .create_function(move |_, ()| {
                        caller_cancel.cancel();
                        Ok(())
                    })
                    .unwrap();
                k.lua().globals().set("CANCEL_CALLER", cancel_fn).unwrap();
                let k_prov = k.clone();
                let cancel_provider_fn = k
                    .lua()
                    .create_function(move |_, ()| {
                        k_prov.st().services.get("cancel-svc").unwrap().lifetime.cancel();
                        Ok(())
                    })
                    .unwrap();
                k.lua().globals().set("CANCEL_PROVIDER", cancel_provider_fn).unwrap();
                let value: Table = k
                    .lua()
                    .load(
                        r#"RELEASED = 0
                           local d = setmetatable({}, { __call = function() RELEASED = RELEASED + 1 end })
                           return {
                               reg_caller = function() CANCEL_CALLER(); return d end,
                               reg_provider = function() CANCEL_PROVIDER(); return d end,
                           }"#,
                    )
                    .eval()
                    .unwrap();
                let options: Table = k
                    .lua()
                    .load(
                        "{ contract = { methods = { reg_caller = { doc = 'r', expose = 'both', registration = true }, \
                         reg_provider = { doc = 'p', expose = 'both', registration = true } } } }",
                    )
                    .eval()
                    .unwrap();
                let admitted = k
                    .admit_service(provider, "cancel-svc", &Value::Table(value.clone()), Some(options), false)
                    .unwrap();
                let _seq = k.register_admitted(provider, "cancel-svc", Value::Table(value), admitted).unwrap();

                let err = k
                    .call_service_export(caller, None, "cancel-svc", "reg_caller", json!({}), CancellationToken::new())
                    .await
                    .unwrap_err();
                assert!(matches!(err, ServiceCallError::CallerDisposed), "{err}");
                assert_eq!(k.lua().load("return RELEASED").eval::<i64>().unwrap(), 1);

                let caller2 = k.open_owner("caller2", "user").unwrap();
                let err = k
                    .call_service_export(
                        caller2,
                        None,
                        "cancel-svc",
                        "reg_provider",
                        json!({}),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(err, ServiceCallError::Unavailable), "{err}");
                assert_eq!(k.lua().load("return RELEASED").eval::<i64>().unwrap(), 2);
            })
            .await;
    }

    #[tokio::test]
    async fn call_service_export_validates_and_returns_raw_result_for_transport_projection() {
        use std::sync::{Arc, atomic::AtomicBool};

        struct NoopByteStream;
        impl mare_shared::wire_projection::ByteStreamProjector for NoopByteStream {
            fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
                chunk.to_vec()
            }
            fn finish(&mut self) -> Vec<u8> {
                Vec::new()
            }
        }

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let flag = Arc::new(AtomicBool::new(true));
                let marker = "[[mare-value:00000000-0000-4000-8000-000000000001]]";
                mare_shared::wire_projection::install(
                    k.lua(),
                    mare_shared::wire_projection::WireProjector::new(
                        flag,
                        Arc::new(move |val| {
                            if let Some(s) = val.get("text").and_then(serde_json::Value::as_str) {
                                val["text"] = serde_json::Value::String(s.replace("S4", marker));
                            }
                        }),
                        Arc::new(move |s| s.replace("S4", marker)),
                        Arc::new(|b| b.to_vec()),
                        Arc::new(|| Box::new(NoopByteStream)),
                    ),
                );
                let caller = k.open_owner("caller", "user").unwrap();
                let provider = k.open_owner("provider", "user").unwrap();
                let value: Table = k
                    .lua()
                    .load(
                        r#"return {
                            short_ok = function() return { text = "S4" } end,
                            too_long = function() return { text = "WRONG" } end,
                        }"#,
                    )
                    .eval()
                    .unwrap();
                let options: Table = k
                    .lua()
                    .load(
                        r#"{ contract = { methods = {
                            short_ok = {
                                doc = "short",
                                expose = "both",
                                result = {
                                    type = "object",
                                    properties = { text = { type = "string", enum = { "S4" } } },
                                    required = { "text" },
                                    additionalProperties = false,
                                },
                            },
                            too_long = {
                                doc = "long",
                                expose = "both",
                                result = {
                                    type = "object",
                                    properties = { text = { type = "string", enum = { "S4" } } },
                                    required = { "text" },
                                    additionalProperties = false,
                                },
                            },
                        } } }"#,
                    )
                    .eval()
                    .unwrap();
                let admitted = k
                    .admit_service(provider, "bounded-svc", &Value::Table(value.clone()), Some(options), false)
                    .unwrap();
                let _seq = k.register_admitted(provider, "bounded-svc", Value::Table(value), admitted).unwrap();

                // Raw value "S4" is validated here; its transport owns the wire projection.
                let out = k
                    .call_service_export(caller, None, "bounded-svc", "short_ok", json!({}), CancellationToken::new())
                    .await
                    .expect("valid raw result must not fail maxLength schema after projection");
                assert_eq!(out["text"], "S4");

                // Genuinely oversized raw value still fails result validation.
                let err = k
                    .call_service_export(caller, None, "bounded-svc", "too_long", json!({}), CancellationToken::new())
                    .await
                    .unwrap_err();
                assert!(matches!(err, ServiceCallError::Result(_)), "{err}");
            })
            .await;
    }

    #[tokio::test]
    async fn allow_provider_reload_guard_resets_flag_on_drop() {
        let cell = Rc::new(Cell::new(false));
        let state = RefCell::new(ExportCallState { allow_provider_reload: cell.clone() });
        EXPORT_CALL
            .scope(state, async {
                assert!(!cell.get());
                {
                    let _guard = allow_provider_reload();
                    assert!(cell.get(), "guard must set allow_provider_reload = true inside scope");
                }
                assert!(!cell.get(), "guard must reset allow_provider_reload = false on drop");
            })
            .await;
    }

    #[tokio::test]
    async fn call_service_export_with_id_and_per_call_token_cancellation_and_update_drain() {
        use std::cell::RefCell;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let caller = k.open_owner("caller", "user").unwrap();
                let provider = k.open_owner("provider", "user").unwrap();
                let work_fn: RefCell<Option<Function>> = RefCell::new(None);
                let holder = Rc::new(work_fn);
                let holder_for_reg = holder.clone();
                let reg_fn = k
                    .lua()
                    .create_function(move |lua, (_self, opts): (Value, Table)| {
                        let cb: Function = opts.get("cb")?;
                        *holder_for_reg.borrow_mut() = Some(cb);
                        lua.create_function(|_, ()| Ok(()))
                    })
                    .unwrap();
                let value = k.lua().create_table().unwrap();
                value.set("reg", reg_fn).unwrap();
                let options: Table = k
                    .lua()
                    .load(
                        r#"{ contract = { methods = {
                            reg = {
                                doc = "register cb",
                                expose = "both",
                                registration = true,
                                order = { "..." },
                                callbacks = {
                                    cb = {
                                        type = "fun(v: string): { ok: boolean }",
                                        params = { type = "object", properties = { v = { type = "string" } }, required = { "v" }, additionalProperties = false },
                                        order = { "v" },
                                        result = { type = "object", properties = { ok = { type = "boolean" } }, required = { "ok" }, additionalProperties = false },
                                        update = { type = "object", properties = { chunk = { type = "string" } }, required = { "chunk" }, additionalProperties = false },
                                    },
                                },
                            },
                        } } }"#,
                    )
                    .eval()
                    .unwrap();
                let admitted = k.admit_service(provider, "cb-svc", &Value::Table(value.clone()), Some(options), false).unwrap();
                let _seq = k.register_admitted(provider, "cb-svc", Value::Table(value), admitted).unwrap();

                #[derive(Default)]
                struct MockChannel {
                    seen_reg: RefCell<Option<String>>,
                    drop_hang: std::rc::Rc<std::cell::Cell<bool>>,
                }
                struct DropFlag(std::rc::Rc<std::cell::Cell<bool>>);
                impl Drop for DropFlag {
                    fn drop(&mut self) {
                        self.0.set(true);
                    }
                }
                impl CallbackChannel for MockChannel {
                    fn call(
                        &self,
                        registration_id: String,
                        _callback: String,
                        _call_id: Option<String>,
                        arguments: Json,
                        update: Option<tokio::sync::mpsc::Sender<Json>>,
                    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Json, ServiceCallError>>>> {
                        *self.seen_reg.borrow_mut() = Some(registration_id);
                        let v = arguments.get("v").and_then(Json::as_str).unwrap_or_default().to_owned();
                        let drop_hang = self.drop_hang.clone();
                        Box::pin(async move {
                            if v == "hang" {
                                let _guard = DropFlag(drop_hang);
                                std::future::pending::<Result<Json, ServiceCallError>>().await
                            } else {
                                if let Some(tx) = update {
                                    let _ = tx.send(json!({ "chunk": "u1" })).await;
                                    let _ = tx.send(json!({ "chunk": "u2" })).await;
                                }
                                Ok(json!({ "ok": true }))
                            }
                        })
                    }
                }
                let ch = Rc::new(MockChannel::default());
                k.set_callback_channel(caller, Some(ch.clone()));

                let out = k
                    .call_service_export_with_id(
                        caller,
                        None,
                        "cb-svc",
                        "reg",
                        json!({ "callbacks": ["cb"] }),
                        CancellationToken::new(),
                        Some("logical-42".into()),
                    )
                    .await
                    .unwrap();
                assert_eq!(out["registration_id"], "logical-42");

                let cb = holder.borrow().clone().unwrap();
                let seen_updates = Rc::new(RefCell::new(Vec::<String>::new()));
                let seen_updates_fn = seen_updates.clone();
                let on_update = k
                    .lua()
                    .create_function(move |_, partial: Table| {
                        seen_updates_fn.borrow_mut().push(partial.get::<String>("chunk")?);
                        Ok(())
                    })
                    .unwrap();
                let token = mare_shared::lua::Token(CancellationToken::new());
                let res: Table = cb.call_async(("run", on_update.clone(), token.clone())).await.unwrap();
                assert!(res.get::<bool>("ok").unwrap());
                assert_eq!(*seen_updates.borrow(), vec!["u1", "u2"]);
                assert_eq!(ch.seen_reg.borrow().as_deref(), Some("logical-42"));

                // Per-call Token cancels an in-flight callback cleanly without cancelling registration.
                let call_cancel = CancellationToken::new();
                let call_token = mare_shared::lua::Token(call_cancel.clone());
                let fut = cb.call_async::<Table>(("hang", on_update, call_token));
                tokio::pin!(fut);
                let wake = tokio::time::sleep(std::time::Duration::from_millis(20));
                tokio::pin!(wake);
                tokio::select! {
                    _ = &mut fut => panic!("hang callback should still be pending"),
                    _ = &mut wake => {}
                }
                call_cancel.cancel();
                let err = fut.await.unwrap_err();
                assert!(err.to_string().contains("cancelled"), "{err}");
                assert!(ch.drop_hang.get(), "cancelled callback future must drop promptly");

                // Registration remains alive for a subsequent call (including trailing nil optional args).
                let res2: Table = cb.call_async(("after", Value::Nil, Value::Nil)).await.unwrap();
                assert!(res2.get::<bool>("ok").unwrap());
            })
            .await;
    }

    #[test]
    fn service_exports_reuses_compiled_invoke_across_methods_and_services() {
        let lua = Lua::new();
        let contracts_tbl: Table = lua
            .load(
                r#"{
                    svc_a = { methods = {
                        first = { doc = "first", expose = "both", params = { type = "object", additionalProperties = false }, result = { type = "boolean" } },
                        second = { doc = "second", expose = "both", params = { type = "object", additionalProperties = false }, result = { type = "boolean" } },
                    } },
                    svc_b = { methods = {
                        other = { doc = "other", expose = "both", params = { type = "object", additionalProperties = false }, result = { type = "boolean" } },
                    } },
                }"#,
            )
            .eval()
            .unwrap();
        let parsed = super::super::contracts::parse_contracts(&lua, Value::Table(contracts_tbl)).unwrap();
        let exp_a =
            ServiceExports::derive(&lua, "svc_a", &std::sync::Arc::new(parsed["svc_a"].clone())).unwrap().unwrap();
        let exp_b =
            ServiceExports::derive(&lua, "svc_b", &std::sync::Arc::new(parsed["svc_b"].clone())).unwrap().unwrap();
        assert!(exp_a.methods.borrow().is_empty(), "derive must not eagerly compile adapters");
        assert!(exp_a.guards.borrow().is_empty(), "derive must not eagerly construct guards");
        let first_a = exp_a.method(&lua, "first").unwrap().unwrap();
        let second_a = exp_a.method(&lua, "second").unwrap().unwrap();
        let other_b = exp_b.method(&lua, "other").unwrap().unwrap();
        let rawequal: Function = lua.globals().get("rawequal").unwrap();
        assert!(
            rawequal.call::<bool>((first_a.invoke.clone(), second_a.invoke.clone())).unwrap(),
            "methods within one service should share a single compiled invoke function"
        );
        assert!(
            rawequal.call::<bool>((first_a.invoke.clone(), other_b.invoke.clone())).unwrap(),
            "services in the same VM should share a single compiled invoke function"
        );
        // Guard lookup on a single method must materialize only that method's guard without
        // compiling adapters for untouched methods.
        assert!(exp_a.guards.borrow().is_empty());
        let g1 = exp_a.guard(&lua, "first").unwrap().unwrap();
        let g1_again = exp_a.guard(&lua, "first").unwrap().unwrap();
        assert!(rawequal.call::<bool>((g1, g1_again)).unwrap(), "guard lookup must cache per method");
        assert_eq!(exp_a.guards.borrow().len(), 1);
        assert!(exp_a.guard(&lua, "missing").unwrap().is_none());
    }

    #[tokio::test]
    async fn call_committed_isolates_identical_ids_across_kernels() {
        let k1 = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let k2 = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        assert_ne!(k1.id(), k2.id(), "distinct kernels must have distinct ids");

        // 1. Plugin provider commit mark must not fire when invoked with another kernel's id,
        //    even when the active owner id matches `provider_owner`.
        let plugin_committed = Rc::new(Cell::new(false));
        CALL_COMMITTED
            .scope(
                CallCommitted {
                    kernel_id: k1.id(),
                    provider_owner: 7,
                    caller: 42,
                    is_host_provider: false,
                    committed: plugin_committed.clone(),
                },
                crate::plugins::prims::in_owner(Some(7), async {
                    mark_service_call_committed(k2.id());
                    assert!(!plugin_committed.get(), "different kernel id must not flip plugin committed");
                    mark_service_call_committed(k1.id());
                    assert!(plugin_committed.get(), "matching kernel id and provider owner must flip committed");
                }),
            )
            .await;

        // 2. Host export direct check and commit mark must both require matching kernel id.
        let host_committed = Rc::new(Cell::new(false));
        CALL_COMMITTED
            .scope(
                CallCommitted {
                    kernel_id: k1.id(),
                    provider_owner: 1,
                    caller: 42,
                    is_host_provider: true,
                    committed: host_committed.clone(),
                },
                crate::plugins::prims::in_owner(Some(42), async {
                    assert!(is_direct_host_export_call(k1.id()));
                    assert!(
                        !is_direct_host_export_call(k2.id()),
                        "different kernel id must not match direct host call"
                    );
                    mark_host_service_call_committed(k2.id());
                    assert!(!host_committed.get(), "different kernel id must not flip host committed");
                    mark_host_service_call_committed(k1.id());
                    assert!(host_committed.get(), "matching kernel id must flip host committed");
                }),
            )
            .await;
    }

    #[tokio::test]
    async fn host_method_registry_and_option_validation_cover_sync_async_and_errors() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let lua = k.lua();

                let bad_opt: Table = lua.load("return { bogus = true }").eval().unwrap();
                assert!(parse_options(lua, "demo", Some(bad_opt)).is_err());

                let essential_opt: Table = lua
                    .load(
                        r#"return { contract = { essential = true, methods = { ping = { doc = "p", expose = "lua", result = { type = "boolean" } } } } }"#,
                    )
                    .eval()
                    .unwrap();
                assert!(parse_options(lua, "demo", Some(essential_opt)).is_err());

                let owned = ServiceContract {
                    required: vec![],
                    optional: vec![],
                    doc: None,
                    methods: BTreeMap::new(),
                    errors: BTreeMap::new(),
                    essential: false,
                    poisoned_allow: None,
                    owner: Some("owner_plugin".into()),
                    events: BTreeMap::new(),
                    ..Default::default()
                };
                assert!(ServiceExports::derive(lua, "owned_demo", &std::sync::Arc::new(owned)).unwrap().is_none());

                let contracts_tbl: Table = lua
                    .load(
                        r#"return {
                            demo_host = {
                                methods = {
                                    sync_op = {
                                        doc = "sync host op",
                                        expose = "both",
                                        provider = "host",
                                        params = { type = "object", properties = { v = { type = "integer" } }, required = { "v" }, additionalProperties = false },
                                        order = { "v" },
                                        result = { type = "object", properties = { kind = { type = "string" }, v = { type = "integer" } }, required = { "kind", "v" }, additionalProperties = false },
                                    },
                                    async_op = {
                                        doc = "async host op",
                                        expose = "both",
                                        provider = "host",
                                        params = { type = "object", properties = { v = { type = "integer" } }, required = { "v" }, additionalProperties = false },
                                        order = { "v" },
                                        result = { type = "object", properties = { kind = { type = "string" }, v = { type = "integer" } }, required = { "kind", "v" }, additionalProperties = false },
                                    },
                                },
                            },
                        }"#,
                    )
                    .eval()
                    .unwrap();
                k.install_service_contracts(contracts_tbl).unwrap();
                let sync_fn = lua
                    .create_function(|lua, v: i64| {
                        let t = lua.create_table()?;
                        t.set("kind", "sync")?;
                        t.set("v", v)?;
                        Ok(t)
                    })
                    .unwrap();
                let async_fn = lua
                    .create_async_function(|lua, v: i64| async move {
                        tokio::task::yield_now().await;
                        let t = lua.create_table()?;
                        t.set("kind", "async")?;
                        t.set("v", v)?;
                        Ok(t)
                    })
                    .unwrap();
                k.register_host_method("demo_host", "sync_op", sync_fn).unwrap();
                k.register_host_method_async("demo_host", "async_op", async_fn).unwrap();
                assert!(k.host_method_entry("demo_host", "sync_op").unwrap().is_some());
                assert!(k.host_method_entry("demo_host", "async_op").unwrap().is_some());
                assert!(k.host_method_entry("demo_host", "missing_op").unwrap().is_none());

                // Boot mounts host-only service contracts via mare.Service and prims.contracts.host_method.
                let mare = lua.create_table().unwrap();
                let task = lua.create_table().unwrap();
                task.set(
                    "token",
                    lua.create_function(|_, ()| Ok(mare_shared::lua::Token(CancellationToken::new()))).unwrap(),
                )
                .unwrap();
                task.set("clock", lua.create_function(|_, ()| Ok(0.0_f64)).unwrap()).unwrap();
                mare.set("task", task).unwrap();
                mare.set("log", lua.create_function(|_, _: mlua::MultiValue| Ok(())).unwrap()).unwrap();
                lua.globals().set("mare", mare).unwrap();
                k.start().await.unwrap();
                let caller = k.open_owner("demo_caller", "user").unwrap();

                let sync_res = k
                    .call_service_export(caller, None, "demo_host", "sync_op", json!({ "v": 1 }), CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(sync_res["kind"], "sync");
                assert_eq!(sync_res["v"], 1);

                let async_res = k
                    .call_service_export(caller, None, "demo_host", "async_op", json!({ "v": 2 }), CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(async_res["kind"], "async");
                assert_eq!(async_res["v"], 2);

                let missing =
                    k.call_service_export(caller, None, "demo_host", "missing_op", json!({}), CancellationToken::new()).await;
                assert!(missing.is_err());
                assert!(k.owner_registrations_matching(999, "demo.").is_empty());
                k.shutdown().await;
            })
            .await;
    }

    #[test]
    fn lua_unchecked_registration_methods_still_enforce_disposer_normalization() {
        let lua = Lua::new();
        let contracts_tbl: Table = lua
            .load(
                r#"return {
                    reg_svc = {
                        methods = {
                            add = {
                                doc = "unchecked registration",
                                expose = "lua",
                                registration = true,
                                lua_unchecked = true,
                                params = { type = "object", additionalProperties = true },
                                order = { "..." },
                            },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        let parsed = super::super::contracts::parse_contracts(&lua, Value::Table(contracts_tbl)).unwrap();
        let exports =
            ServiceExports::derive(&lua, "reg_svc", &std::sync::Arc::new(parsed["reg_svc"].clone())).unwrap().unwrap();
        let g = exports.guard(&lua, "add").unwrap().unwrap();
        assert!(!g.get::<bool>("passthrough").unwrap(), "registration methods must not bypass leave");
        assert!(g.get::<bool>("keep_extra").unwrap(), "unchecked registrations preserve extra return values");
        assert!(g.get::<Option<Function>>("enter").unwrap().is_none(), "lua_unchecked skips enter");
        let leave: Function = g.get("leave").unwrap();

        // 1. Non-disposer return (e.g. integer or bare nil) is rejected with invalid_result.
        let packed_bad: Table = lua.load("return table.pack(42)").eval().unwrap();
        let (val_bad, err_bad): (Value, Value) = leave.call(packed_bad).unwrap();
        assert!(val_bad.is_nil());
        assert!(err_bad.as_table().unwrap().get::<String>("code").unwrap() == "invalid_result");

        let packed_nil: Table = lua.load("return table.pack()").eval().unwrap();
        let (val_nil, err_nil): (Value, Value) = leave.call(packed_nil).unwrap();
        assert!(val_nil.is_nil());
        assert!(err_nil.as_table().unwrap().get::<String>("code").unwrap() == "invalid_result");

        // 2. Soft-fail `(nil, err)` is preserved without raising.
        let packed_soft_err: Table = lua.load("return table.pack(nil, 'bad entry')").eval().unwrap();
        let (val_soft, err_soft): (Value, Value) = leave.call(packed_soft_err).unwrap();
        assert!(val_soft.is_nil());
        assert!(err_soft.is_nil());

        // 2. Callable table disposer is normalized to a function and still invokes __call.
        let packed_tbl: Table = lua
            .load(
                r#"CALLED = 0
                local disp = setmetatable({}, { __call = function() CALLED = CALLED + 1 end })
                return table.pack(disp)"#,
            )
            .eval()
            .unwrap();
        let (val_ok, err_ok): (Value, Value) = leave.call(packed_tbl).unwrap();
        assert!(err_ok.is_nil());
        let disposer = val_ok.as_function().expect("callable table must be normalized to a function");
        disposer.call::<()>(()).unwrap();
        assert_eq!(lua.globals().get::<i64>("CALLED").unwrap(), 1);
    }

    #[tokio::test]
    async fn tool_body_calling_exported_service_retains_exec_ctx_after_call_binding_drop() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let lua = k.lua();
                let provider = k.open_owner("demo_provider", "user").unwrap();
                let caller = k.open_owner("demo_caller", "user").unwrap();

                let options_tbl: Table = lua
                    .load(
                        r#"return {
                            contract = {
                                methods = {
                                    ping = {
                                        doc = "echo active tool tag",
                                        expose = "both",
                                        result = { type = "string" },
                                    },
                                },
                            },
                        }"#,
                    )
                    .eval()
                    .unwrap();
                let impl_tbl = lua.create_table().unwrap();
                impl_tbl
                    .set(
                        "ping",
                        lua.create_function(|lua, _self: Value| {
                            Ok(mare_shared::exec_ctx::current(lua)
                                .and_then(|t| t.get::<String>("tool").ok())
                                .unwrap_or_else(|| "none".to_string()))
                        })
                        .unwrap(),
                    )
                    .unwrap();
                let val = Value::Table(impl_tbl);
                let admitted = k.admit_service(provider, "demo_rpc", &val, Some(options_tbl), false).unwrap();
                k.register_admitted(provider, "demo_rpc", val, admitted).unwrap();

                // Expose `invoke_export()` to Lua so a tool body running inside `exec_ctx::run_in`
                // can invoke `call_service_export` and verify its own tag survives CallBinding drop.
                let k_call = k.clone();
                let invoke_export = lua
                    .create_async_function(move |_, ()| {
                        let k_call = k_call.clone();
                        async move {
                            let res = k_call
                                .call_service_export(
                                    caller,
                                    None,
                                    "demo_rpc",
                                    "ping",
                                    json!({}),
                                    CancellationToken::new(),
                                )
                                .await
                                .map_err(|e| mlua::Error::runtime(format!("{e:?}")))?;
                            Ok(res.as_str().unwrap_or_default().to_string())
                        }
                    })
                    .unwrap();
                let current_tag = lua
                    .create_function(|lua, ()| {
                        Ok(mare_shared::exec_ctx::current(lua)
                            .and_then(|t| t.get::<String>("tool").ok())
                            .unwrap_or_else(|| "none".to_string()))
                    })
                    .unwrap();

                let tool_body: Function = lua
                    .load(
                        r#"local invoke_export, current_tag = ...
                        return function()
                            local before = current_tag()
                            local during = invoke_export()
                            local after = current_tag()
                            return before, during, after
                        end"#,
                    )
                    .call((invoke_export, current_tag))
                    .unwrap();
                let tool_ctx = lua.create_table().unwrap();
                tool_ctx.set("tool", "bash").unwrap();
                let vals =
                    mare_shared::exec_ctx::run_in(lua, tool_ctx, tool_body, mlua::MultiValue::new()).await.unwrap();
                let mut it = vals.into_iter().map(|v| match v {
                    Value::String(s) => s.to_str().unwrap().to_string(),
                    other => panic!("expected string, got {other:?}"),
                });
                assert_eq!(it.next().as_deref(), Some("bash"), "tag before service call");
                assert_eq!(it.next().as_deref(), Some("bash"), "tag propagated into exported service");
                assert_eq!(
                    it.next().as_deref(),
                    Some("bash"),
                    "tool body must retain its exec_ctx tag after CallBinding drops"
                );

                // Untagged caller remains untagged after CallBinding drops.
                let untagged = k
                    .call_service_export(caller, None, "demo_rpc", "ping", json!({}), CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(untagged, json!("none"));
                assert!(mare_shared::exec_ctx::current(lua).is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn committed_lua_provider_completes_when_caller_disposed() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let lua = k.lua();
                let provider = k.open_owner("demo_commit_provider", "user").unwrap();
                let options_tbl: Table = lua
                    .load(
                        r#"return {
                            contract = {
                                methods = {
                                    write = {
                                        doc = "commit-guarded write",
                                        expose = "both",
                                        params = { type = "object", properties = { commit = { type = "boolean" } }, required = { "commit" }, additionalProperties = false },
                                        order = { "commit" },
                                        result = { type = "boolean" },
                                    },
                                },
                            },
                        }"#,
                    )
                    .eval()
                    .unwrap();

                let entered = std::rc::Rc::new(tokio::sync::Notify::new());
                let release = std::rc::Rc::new(tokio::sync::Notify::new());
                let published = std::rc::Rc::new(Cell::new(false));
                let k_id = k.id();
                let entered_fn = entered.clone();
                let release_fn = release.clone();
                let published_fn = published.clone();
                let impl_tbl = lua.create_table().unwrap();
                impl_tbl
                    .set(
                        "write",
                        lua.create_async_function(move |_, (_self, commit): (Value, bool)| {
                            let entered = entered_fn.clone();
                            let release = release_fn.clone();
                            let published = published_fn.clone();
                            async move {
                                if commit {
                                    crate::plugins::prims::in_owner(Some(provider), async {
                                        mark_service_call_committed(k_id);
                                    })
                                    .await;
                                }
                                entered.notify_waiters();
                                release.notified().await;
                                published.set(true);
                                Ok(true)
                            }
                        })
                        .unwrap(),
                    )
                    .unwrap();
                let val = Value::Table(impl_tbl);
                let admitted = k.admit_service(provider, "demo_commit", &val, Some(options_tbl), false).unwrap();
                k.register_admitted(provider, "demo_commit", val, admitted).unwrap();

                // 1. Committed Lua provider call must finish publishing even when caller ctx is disposed.
                let caller1 = k.open_owner("rpc-conn-1", "user").unwrap();
                let k1 = k.clone();
                let task1 = tokio::task::spawn_local(async move {
                    k1.call_service_export(
                        caller1,
                        None,
                        "demo_commit",
                        "write",
                        json!({ "commit": true }),
                        CancellationToken::new(),
                    )
                    .await
                });
                entered.notified().await;
                let _ = k.dispose_ctx_defer_callbacks(caller1).await;
                release.notify_waiters();
                let res1 = task1.await.unwrap();
                assert_eq!(res1.expect("committed Lua call must finish after caller_cancel"), json!(true));
                assert!(published.get(), "post-commit publish must run to completion");

                // 2. Uncommitted Lua provider call is still aborted with CallerDisposed when caller ctx is disposed.
                published.set(false);
                let caller2 = k.open_owner("rpc-conn-2", "user").unwrap();
                let k2 = k.clone();
                let task2 = tokio::task::spawn_local(async move {
                    k2.call_service_export(
                        caller2,
                        None,
                        "demo_commit",
                        "write",
                        json!({ "commit": false }),
                        CancellationToken::new(),
                    )
                    .await
                });
                entered.notified().await;
                let _ = k.dispose_ctx_defer_callbacks(caller2).await;
                let res2 = task2.await.unwrap();
                assert!(
                    matches!(res2, Err(ServiceCallError::CallerDisposed)),
                    "uncommitted Lua call must still abort with CallerDisposed: {res2:?}"
                );
                assert!(!published.get(), "uncommitted call must not run post-barrier code");
            })
            .await;
    }
}
