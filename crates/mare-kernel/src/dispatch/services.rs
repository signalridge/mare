//! The service registry: admission and registration of mare.Service providers, and lookups.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use mlua::{Table, Value};

use crate::{Kernel, Origin, ServiceContract, ServiceEntry};

/// A value admitted under its contract, not registered yet ([`Kernel::admit_service`]).
pub(crate) struct Admitted {
    contract: Arc<ServiceContract>,
    exports: Option<std::rc::Rc<super::service_rpc::ServiceExports>>,
    unconfined: bool,
    implemented: BTreeSet<String>,
}

impl Admitted {
    /// The plugin-implemented methods the contract declares for Lua callers (`lua` or `both`).
    pub(crate) fn lua_methods(&self) -> Vec<String> {
        self.contract.methods.iter().filter(|(_, spec)| !spec.is_host()).map(|(name, _)| name.clone()).collect()
    }

    /// The host-provided methods (`provider = "host"`) declared for Lua callers (`lua` or `both`).
    pub(crate) fn host_lua_methods(&self) -> Vec<String> {
        self.contract.methods.iter().filter(|(_, spec)| spec.is_host()).map(|(name, _)| name.clone()).collect()
    }

    /// Shared derived exports handle for lazy per-method guard construction.
    pub(crate) fn exports(&self) -> Option<std::rc::Rc<super::service_rpc::ServiceExports>> {
        self.exports.clone()
    }

    /// The contract's `registration = true` methods.
    pub(crate) fn registrations(&self) -> Vec<String> {
        self.contract.methods.iter().filter(|(_, spec)| spec.registration).map(|(name, _)| name.clone()).collect()
    }
}

pub(crate) use super::contracts::parse_contracts;

fn contract_for<'a>(st: &'a crate::State, service: &str) -> Option<&'a ServiceContract> {
    st.contracts
        .get(service)
        .map(Arc::as_ref)
        .or_else(|| st.defs.iter().find_map(|d| d.contracts.get(service)))
        .or_else(|| st.live_service(service).map(|entry| entry.contract.as_ref()))
}

impl Kernel {
    /// Record or clear a problem with a plugin's service contract.
    fn contract_problem(&self, plugin: &str, name: &str, error: Option<&str>) {
        let prefix = format!("plugin {plugin} service contract `{name}`: ");
        let mut st = self.st();
        st.problems.retain(|p| !p.starts_with(&prefix));
        if let Some(error) = error {
            st.problems.push(format!("{prefix}{error}"));
        }
    }

    /// Install built-in contracts before plugins start; they cannot be replaced by providers.
    pub fn install_service_contracts(&self, contracts: Table) -> mlua::Result<()> {
        let contracts =
            parse_contracts(self.lua(), Value::Table(contracts))?.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
        self.install_parsed_service_contracts(Arc::new(contracts))
    }

    /// Install already-parsed built-in contracts before plugins start (avoids parsing or cloning
    /// the manifest when the host also inspects policy points before `Kernel::new`).
    pub fn install_parsed_service_contracts(
        &self,
        contracts: Arc<BTreeMap<String, Arc<ServiceContract>>>,
    ) -> mlua::Result<()> {
        for (name, contract) in contracts.as_ref() {
            if let Some(owner) = &contract.owner
                && !contracts.contains_key(owner)
            {
                return Err(mlua::Error::runtime(format!(
                    "event-only contract `{name}` names unknown built-in owner service `{owner}`"
                )));
            }
        }
        {
            let mut st = self.st();
            if !st.services.is_empty() || !st.contracts.is_empty() {
                return Err(mlua::Error::runtime("service contracts must be installed before services"));
            }
            st.contracts = contracts;
            Self::rebuild_event_catalog_locked(&mut st);
        }
        super::policy_prims::sync_points(self);
        self.sync_event_log_contracts();
        Ok(())
    }

    pub(crate) fn sync_event_log_contracts(&self) {
        let map = {
            let st = self.st();
            let mut map = std::collections::HashMap::new();
            for spec in st
                .contracts
                .values()
                .map(Arc::as_ref)
                .chain(st.defs.iter().flat_map(|def| def.contracts.values()))
                .flat_map(|contract| contract.events.values())
            {
                map.entry(spec.name.clone()).or_insert(super::tap::ContractTapMeta {
                    reach: spec.expose,
                    scope: spec.scope,
                    delivery: spec.delivery,
                    ignorable: spec.ignorable,
                    retain: spec.retain.clone(),
                });
            }
            map
        };
        self.0.event_log.set_contract_events(map);
        self.taps_changed();
    }

    pub(crate) fn rebuild_event_catalog_locked(st: &mut crate::State) {
        st.policy_metadata.clear();
        st.policy_points.clear();
        st.rewrite_points.clear();
        for spec in st
            .contracts
            .values()
            .map(Arc::as_ref)
            .chain(st.defs.iter().flat_map(|def| def.contracts.values()))
            .flat_map(|contract| contract.events.values())
        {
            if spec.mode == mare_protocol::EventMode::Policy {
                st.policy_points.insert(spec.name.clone());
                if let Some(meta) = spec.policy_metadata() {
                    st.policy_metadata.insert(spec.name.clone(), meta);
                }
            } else if spec.mode == mare_protocol::EventMode::Rewrite {
                st.rewrite_points.insert(spec.name.clone());
            }
        }
    }

    /// Look up one active contract event declaration (`contracts.<root>.events`) by full name.
    pub fn event_spec(&self, event: &str) -> Option<super::contracts::EventSpec> {
        let st = self.st();
        st.contracts
            .values()
            .map(Arc::as_ref)
            .chain(st.defs.iter().flat_map(|def| def.contracts.values()))
            .flat_map(|contract| contract.events.values())
            .find(|spec| spec.name == event)
            .cloned()
    }

    /// Whether `event` is declared `expose = "process"` in any active contract.
    pub fn is_process_event(&self, event: &str) -> bool {
        self.event_spec(event).is_some_and(|spec| spec.expose == super::contracts::EventReach::Process)
    }

    /// Whether `event` is public via `PROTOCOL_EVENTS` or a contract declaration with `expose = "both"`.
    pub fn is_public_declared_event(&self, event: &str) -> bool {
        !self.is_process_event(event)
            && (mare_protocol::event_registry::is_protocol_event(event)
                || self.event_spec(event).is_some_and(|spec| spec.expose == super::contracts::EventReach::Both))
    }

    /// Delivery class (`required` vs `lossy`) of a protocol or contract event.
    pub fn event_delivery(&self, event: &str) -> Option<mare_protocol::EventDelivery> {
        self.0.event_log.event_delivery(event)
    }

    /// Whether clients may ignore an unrecognized event of this name (`ignorable = true`).
    pub fn event_ignorable(&self, event: &str) -> bool {
        self.0.event_log.event_ignorable(event)
    }

    /// Asynchronous remote hook mode declared by `PROTOCOL_EVENTS` or an active contract
    /// (`emit|parallel|serial|waterfall`). Returns `None` for non-declared events,
    /// `expose = process`, `mode = bail`, or `mode = policy`
    /// (policy points attach as voters via `mode = "policy"`).
    pub fn declared_remote_hook_mode(&self, event: &str) -> Option<&'static str> {
        if let Some(def) = mare_protocol::event_registry::find(event) {
            return match def.mode {
                mare_protocol::EventMode::Emit => Some("emit"),
                mare_protocol::EventMode::Parallel => Some("parallel"),
                mare_protocol::EventMode::Serial => Some("serial"),
                mare_protocol::EventMode::Waterfall => Some("waterfall"),
                mare_protocol::EventMode::Bail
                | mare_protocol::EventMode::Policy
                | mare_protocol::EventMode::Rewrite => None,
            };
        }
        let spec = self.event_spec(event)?;
        if spec.expose == super::contracts::EventReach::Process {
            return None;
        }
        if spec.mode == mare_protocol::EventMode::Rewrite {
            return Some("waterfall");
        }
        match spec.mode {
            mare_protocol::EventMode::Emit => Some("emit"),
            mare_protocol::EventMode::Parallel => Some("parallel"),
            mare_protocol::EventMode::Serial => Some("serial"),
            mare_protocol::EventMode::Waterfall => Some("waterfall"),
            mare_protocol::EventMode::Bail | mare_protocol::EventMode::Policy | mare_protocol::EventMode::Rewrite => {
                None
            }
        }
    }

    /// Runtime dispatch gate: verify that under any registered contract/protocol root,
    /// the event is declared, the dispatch verb matches the declared mode, and Lua callers
    /// belong to the declaring owner service/plugin.
    pub(crate) fn validate_event_dispatch(&self, verb: &str, event: &str, bus_owner: Option<u64>) -> mlua::Result<()> {
        self.validate_event_dispatch_caller(verb, event, bus_owner.map(Some))
    }

    pub(crate) fn validate_event_dispatch_caller(
        &self,
        verb: &str,
        event: &str,
        bus_owner: Option<Option<u64>>,
    ) -> mlua::Result<()> {
        fn verb_matches(verb: &str, mode: mare_protocol::EventMode) -> bool {
            match verb {
                "emit" | "emit_checked" => {
                    matches!(
                        mode,
                        mare_protocol::EventMode::Emit
                            | mare_protocol::EventMode::Parallel
                            | mare_protocol::EventMode::Serial
                    )
                }
                "bail" => matches!(mode, mare_protocol::EventMode::Bail | mare_protocol::EventMode::Serial),
                "parallel" => matches!(mode, mare_protocol::EventMode::Emit | mare_protocol::EventMode::Parallel),
                "serial" => matches!(mode, mare_protocol::EventMode::Emit | mare_protocol::EventMode::Serial),
                "waterfall" | "waterfall_checked" => mode == mare_protocol::EventMode::Waterfall,
                "policy" => mode == mare_protocol::EventMode::Policy,
                "rewrite" => mode == mare_protocol::EventMode::Rewrite,
                _ => false,
            }
        }

        if let Some(def) = mare_protocol::event_registry::find(event) {
            if !verb_matches(verb, def.mode) {
                return Err(mlua::Error::runtime(format!(
                    "event `{event}` has mode `{:?}`, cannot dispatch with `{verb}`",
                    def.mode
                )));
            }
            return Ok(());
        }

        let st = self.st();
        let spec = st
            .contracts
            .values()
            .map(Arc::as_ref)
            .chain(st.defs.iter().flat_map(|def| def.contracts.values()))
            .flat_map(|contract| contract.events.values())
            .find(|s| s.name == event);

        let Some(spec) = spec else {
            return Err(mlua::Error::runtime(format!(
                "undeclared event `{event}` cannot be dispatched (declare it in a contract)"
            )));
        };

        if !verb_matches(verb, spec.mode) {
            return Err(mlua::Error::runtime(format!(
                "event `{event}` has mode `{:?}`, cannot dispatch with `{verb}`",
                spec.mode
            )));
        }

        let caller_id = match bus_owner {
            Some(resolved) => resolved,
            None => crate::plugins::prims::current_owner(),
        };
        if let Some(caller_id) = caller_id
            && let Some(caller_ctx) = st.ctxs.get(&caller_id)
            && caller_ctx.origin != "root"
        {
            let caller_plugin = caller_ctx.plugin.as_str();
            let owner_provider_ctx = st.live_service(&spec.owner).map(|e| e.ctx);
            let owner_provider_plugin = owner_provider_ctx.and_then(|cid| st.ctxs.get(&cid)).map(|c| c.plugin.as_str());
            let direct_owner = owner_provider_ctx == Some(caller_id)
                || owner_provider_plugin == Some(caller_plugin)
                || caller_plugin == spec.owner;
            let listed_emitter = spec.emitters.iter().any(|pat| {
                pat.strip_suffix('*').map_or(caller_plugin == pat, |prefix| caller_plugin.starts_with(prefix))
            });
            if !direct_owner && !listed_emitter {
                return Err(mlua::Error::runtime(format!(
                    "plugin `{caller_plugin}` cannot dispatch `{event}` owned by `{}`",
                    spec.owner
                )));
            }
        }
        Ok(())
    }

    /// Runtime event declarations merged across `protocol_events!`, built-in contracts, and
    /// currently loaded plugin contracts.
    pub fn list_events(&self) -> Vec<mare_protocol::EventInfo> {
        let mut out = mare_protocol::event_registry::list_events();
        let mut seen: BTreeSet<String> = out.iter().map(|info| info.name.clone()).collect();
        let st = self.st();
        for spec in st
            .contracts
            .values()
            .map(Arc::as_ref)
            .chain(st.defs.iter().flat_map(|def| def.contracts.values()))
            .flat_map(|contract| contract.events.values())
        {
            if seen.insert(spec.name.clone()) {
                out.push(spec.to_info());
            }
        }
        out
    }

    /// Currently active contract event specs (`contracts.<root>.events`) keyed by full event name.
    pub fn contract_events(&self) -> std::collections::BTreeMap<String, super::contracts::EventSpec> {
        let mut out = std::collections::BTreeMap::new();
        let st = self.st();
        for spec in st
            .contracts
            .values()
            .map(Arc::as_ref)
            .chain(st.defs.iter().flat_map(|def| def.contracts.values()))
            .flat_map(|contract| contract.events.values())
        {
            out.entry(spec.name.clone()).or_insert_with(|| spec.clone());
        }
        out
    }

    /// Look up the declared `EventScope` of a loaded contract event, if any.
    pub(crate) fn contract_event_scope(&self, event: &str) -> Option<mare_protocol::EventScope> {
        let st = self.st();
        st.contracts
            .values()
            .map(Arc::as_ref)
            .chain(st.defs.iter().flat_map(|def| def.contracts.values()))
            .flat_map(|contract| contract.events.values())
            .find(|spec| spec.name == event)
            .map(|spec| spec.scope)
    }

    /// The contract a new provider of `name` must implement: built in, declared by a plugin, or
    /// the one the current runtime provider registered. None when the name is new.
    pub fn declared_contract(&self, name: &str) -> Option<ServiceContract> {
        let st = self.st();
        contract_for(&st, name).cloned()
    }

    /// Essential built-in services from the installed contracts (third-party contracts cannot mark
    /// themselves essential).
    pub fn essential_services(&self) -> Vec<String> {
        self.st().contracts.iter().filter(|(_, c)| c.essential).map(|(name, _)| name.clone()).collect()
    }

    /// Whether `service.method` may run while the session barrier is poisoned. Services without a
    /// `poisoned` allowlist pass; guarded ones allow only methods marked `poisoned = "allow"`.
    pub fn allows_when_poisoned(&self, service: &str, method: &str) -> bool {
        let st = self.st();
        match contract_for(&st, service).and_then(|c| c.poisoned_allow.as_ref()) {
            Some(allowed) => allowed.contains(method),
            None => true,
        }
    }

    /// The methods `service`'s contract allows while the session barrier is poisoned (`None` when
    /// the service is not guarded by a `poisoned` allowlist).
    pub fn poisoned_allowed_methods(&self, service: &str) -> Option<BTreeSet<String>> {
        let st = self.st();
        contract_for(&st, service).and_then(|c| c.poisoned_allow.clone())
    }

    /// A dry run of a provide's admission (the same checks, nothing registered): a caller that
    /// replaces its own registration of `name` runs it before withdrawing the old one, so a
    /// replacement the admission refuses (a declared contract's required method missing, a bad
    /// schema or `expose`) leaves the old provider serving. `replacing`: the caller's own current
    /// registration goes first, so its runtime contract does not bind the new one.
    pub fn check_admission(
        &self,
        ctx: u64,
        name: &str,
        value: &Value,
        options: Option<Table>,
        replacing: bool,
    ) -> mlua::Result<()> {
        self.admit_service(ctx, name, value, options, replacing).map(|_| ())
    }

    /// The admission half of a provide (`prims.contracts.admit`): the contract is selected
    /// (declared, or the runtime one, never replaced by a different one) and the value is checked
    /// against it. Nothing is registered. `replacing`: the current provider is the caller's own,
    /// withdrawn before this one registers (its runtime contract does not bind this one).
    pub(crate) fn admit_service(
        &self,
        ctx: u64,
        name: &str,
        value: &Value,
        options: Option<Table>,
        replacing: bool,
    ) -> mlua::Result<Admitted> {
        let options = super::service_rpc::parse_options(self.lua(), name, options)?;
        let (plugin, contract) = {
            let mut st = self.st();
            let plugin = match st.ctxs.get(&ctx) {
                Some(c) if !c.disposed => c.plugin.clone(),
                _ => return Err(mlua::Error::runtime("ctx is disposed")),
            };
            if plugin != "<root>" {
                let names = st.provided.entry(plugin.clone()).or_default();
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
            if options.contract.is_none()
                && plugin != "<root>"
                && let Some(d) = st.defs.iter().find(|d| d.name == plugin)
                && !d.provide.iter().any(|p| p == name)
            {
                return Err(mlua::Error::runtime(format!(
                    "plugin `{plugin}` provides `{name}` but does not declare it in `provide`"
                )));
            }
            let builtin_decl = st.contracts.get(name).map(Arc::clone);
            let plugin_decls: Vec<&ServiceContract> = st.defs.iter().filter_map(|d| d.contracts.get(name)).collect();
            let total_decls = usize::from(builtin_decl.is_some()) + plugin_decls.len();
            let previous = st.live_service(name).filter(|_| !replacing).map(|entry| Arc::clone(&entry.contract));
            let same = |one: &ServiceContract, two: &ServiceContract| {
                one.required.iter().collect::<BTreeSet<_>>() == two.required.iter().collect::<BTreeSet<_>>()
                    && one.optional.iter().collect::<BTreeSet<_>>() == two.optional.iter().collect::<BTreeSet<_>>()
                    && one.methods == two.methods
                    && one.errors == two.errors
                    && one.essential == two.essential
                    && one.poisoned_allow == two.poisoned_allow
                    && one.owner == two.owner
                    && one.events == two.events
            };
            let contract: Result<Arc<ServiceContract>, String> = match (total_decls, options.contract.as_ref()) {
                (1, proposed) => {
                    let one = builtin_decl.unwrap_or_else(|| Arc::new(plugin_decls[0].clone()));
                    if one.owner.is_some() {
                        Err(format!(
                            "`{name}` is an event-only contract owned by `{}`",
                            one.owner.as_deref().unwrap_or_default()
                        ))
                    } else if let Some(proposed) = proposed
                        && !same(&one, proposed)
                    {
                        Err(format!("service `{name}` contract cannot be replaced"))
                    } else {
                        Ok(one)
                    }
                }
                (0, Some(proposed)) if previous.as_ref().is_some_and(|previous| !same(previous, proposed)) => {
                    Err(format!("service `{name}` runtime contract cannot be replaced"))
                }
                (0, Some(proposed)) => Ok(Arc::new(proposed.clone())),
                (0, None) => previous.ok_or_else(|| format!("service `{name}` has no declared contract")),
                _ => Err(format!("service `{name}` has multiple declared contracts")),
            };
            (plugin, contract)
        };
        let contract = match contract {
            Ok(contract) => contract,
            Err(message) => {
                self.contract_problem(&plugin, name, Some(&message));
                return Err(mlua::Error::runtime(message));
            }
        };
        let exports = self.derived_exports(name, &contract)?;
        // Table::get performs Lua indexing, including __index. Do not hold the kernel lock:
        // an __index function may call back into the kernel.
        let validate = || -> mlua::Result<BTreeSet<String>> {
            let Value::Table(table) = value else {
                return Err(mlua::Error::runtime(format!("service `{name}` must be a table")));
            };
            for method in &contract.required {
                if !matches!(table.get::<Value>(method.as_str())?, Value::Function(_)) {
                    return Err(mlua::Error::runtime(format!(
                        "service `{name}` requires method `{method}` (function)"
                    )));
                }
            }
            let mut implemented: BTreeSet<String> = contract.required.iter().cloned().collect();
            for (method, spec) in &contract.methods {
                if spec.is_host() {
                    implemented.insert(method.clone());
                }
            }
            for method in &contract.optional {
                match table.get::<Value>(method.as_str())? {
                    Value::Nil => {}
                    Value::Function(_) => {
                        implemented.insert(method.clone());
                    }
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                            "service `{name}` optional method `{method}` must be a function"
                        )));
                    }
                }
            }
            // the provider's layers, most derived first: the table, then (a mare.Service) its class
            // and the class's `super` chain up to mare.Service, or (a plain table) the prototype
            // tables its `__index` chain names. A cordis class's `__index` is a function (its
            // accessors), so the class chain is followed through `super`; it stops at mare.Service by
            // identity (the table, rawequal), never by name. In a class layer the
            // framework's own keys (`__*`, `new`, `is`, `constructor`) are not methods
            // A chain that repeats a table, or one deeper than LAYERS, is refused: never admitted
            // with only the layers seen so far checked.
            const LAYERS: usize = 16;
            let base: Option<Table> = self.lua().named_registry_value(super::contract_prims::SERVICE_BASE).ok();
            let mut layers: Vec<(Table, bool)> = vec![(table.clone(), false)];
            let mut seen = std::collections::HashSet::from([table.to_pointer()]);
            let too_deep =
                || mlua::Error::runtime(format!("service `{name}`: its class chain is deeper than {LAYERS} layers"));
            // a cordis class is marked `__class` (cordis.class)
            let class =
                table.metatable().filter(|meta| matches!(meta.raw_get::<Value>("__class"), Ok(Value::Boolean(true))));
            let is_class = class.is_some();
            let mut current = match class {
                Some(class) => Some(class),
                None => table.metatable().and_then(|meta| meta.raw_get::<Option<Table>>("__index").ok().flatten()),
            };
            while let Some(layer) = current.take() {
                if base.as_ref().is_some_and(|base| base.to_pointer() == layer.to_pointer()) {
                    break;
                }
                if !seen.insert(layer.to_pointer()) {
                    return Err(mlua::Error::runtime(format!("service `{name}`: its class chain repeats a table")));
                }
                if layers.len() > LAYERS {
                    return Err(too_deep());
                }
                current = if is_class {
                    layer.raw_get::<Option<Table>>("super").ok().flatten()
                } else {
                    layer.metatable().and_then(|meta| meta.raw_get::<Option<Table>>("__index").ok().flatten())
                };
                layers.push((layer, is_class));
            }
            // ... and a `_`-prefixed method is the provider's own helper (the private-name convention
            // of), not public surface
            let framework = |key: &str| key.starts_with('_') || matches!(key, "new" | "is" | "constructor");
            // Every service contract is the whole public surface: a function it does not declare is
            // refused, and a host-provided method cannot be implemented or overridden by a plugin.
            let mut undeclared = BTreeSet::new();
            let mut host_overrides = BTreeSet::new();
            for (layer, class) in &layers {
                for pair in layer.clone().pairs::<Value, Value>() {
                    let (key, value) = pair?;
                    if let (Value::String(key), Value::Function(_)) = (&key, &value) {
                        let key = key.to_string_lossy();
                        if *class && framework(&key) {
                            continue;
                        }
                        match contract.methods.get(&key) {
                            Some(spec) if spec.is_host() => {
                                host_overrides.insert(format!("`{key}`"));
                            }
                            Some(_) => {}
                            None => {
                                undeclared.insert(format!("`{key}`"));
                            }
                        }
                    }
                }
            }
            if !host_overrides.is_empty() {
                let list = host_overrides.into_iter().collect::<Vec<_>>().join(", ");
                return Err(mlua::Error::external(mare_shared::errors::Coded {
                    code: "invalid_arguments",
                    message: format!(
                        "service `{name}` method(s) {list} are host-provided (`provider = \"host\"`) and cannot be implemented or overridden by a plugin"
                    ),
                }));
            }
            if !undeclared.is_empty() {
                let one = undeclared.len() == 1;
                let list = undeclared.into_iter().collect::<Vec<_>>().join(", ");
                let (noun, verb, them) = if one { ("method", "is", "it") } else { ("methods", "are", "them") };
                return Err(mlua::Error::runtime(format!(
                    "service `{name}` {noun} {list} {verb} not in its contract (declare {them}, or keep {them} local to the provider)"
                )));
            }
            Ok(implemented)
        };
        let implemented = match validate() {
            Ok(implemented) => implemented,
            Err(e) => {
                self.contract_problem(&plugin, name, Some(&e.to_string()));
                return Err(e);
            }
        };
        self.contract_problem(&plugin, name, None);
        Ok(Admitted { contract, exports, unconfined: options.unconfined, implemented })
    }

    /// How many service names keep derived exports (tests: the cache follows the providers).
    #[doc(hidden)]
    pub fn derived_exports_len(&self) -> usize {
        self.st().derived.len()
    }

    /// The identity of service `name`'s derived exports (tests: a reload reuses them).
    #[doc(hidden)]
    pub fn derived_exports_id(&self, name: &str) -> Option<usize> {
        self.st().derived.get(name).map(|(_, exports)| std::rc::Rc::as_ptr(exports) as usize)
    }

    /// [`super::service_rpc::ServiceExports::derive`] once per contract of a service name: the
    /// cached derivation when the contract is the same; a new one is kept only once its provider
    /// registers ([`Self::register_admitted`]), so a refused admission leaves nothing behind.
    fn derived_exports(
        &self,
        name: &str,
        contract: &Arc<ServiceContract>,
    ) -> mlua::Result<Option<std::rc::Rc<super::service_rpc::ServiceExports>>> {
        if let Some((derived_for, exports)) = self.st().derived.get(name)
            && (Arc::ptr_eq(derived_for, contract) || derived_for == contract)
        {
            return Ok(Some(exports.clone()));
        }
        let Some(exports) = super::service_rpc::ServiceExports::derive(self.lua(), name, contract)? else {
            return Ok(None);
        };
        Ok(Some(std::rc::Rc::new(exports)))
    }

    /// The registry half: `name` is provided by `ctx` with what [`Kernel::admit_service`] admitted.
    pub(crate) fn register_admitted(
        &self,
        ctx: u64,
        name: &str,
        value: Value,
        admitted: Admitted,
    ) -> mlua::Result<u64> {
        let Admitted { contract, exports, unconfined, implemented } = admitted;
        let mut st = self.st();
        if !st.ctxs.get(&ctx).is_some_and(|c| !c.disposed) {
            return Err(mlua::Error::runtime("ctx is disposed"));
        }
        if st.live_service(name).is_some() {
            // the Lua slot check (calls.refuse_taken) refuses first; this keeps the invariant
            return Err(mlua::Error::external(mare_shared::errors::Coded {
                code: "already_provided",
                message: format!("service `{name}` is already provided"),
            }));
        }
        if let Some(exports) = &exports
            && !st.derived.get(name).is_some_and(|(_, kept)| std::rc::Rc::ptr_eq(kept, exports))
        {
            st.derived.insert(name.to_string(), (contract.clone(), exports.clone()));
        }
        let seq = st.next_id;
        st.next_id += 1;
        st.services.insert(
            name.to_string(),
            ServiceEntry {
                ctx,
                seq,
                value,
                contract,
                lifetime: tokio_util::sync::CancellationToken::new(),
                exports,
                unconfined,
                implemented,
            },
        );
        drop(st);
        Ok(seq)
    }

    /// Built-in service contracts whose methods are all `provider = "host"` (whole-service host
    /// providers such as `baseline`, mounted automatically by boot before plugins start).
    pub(crate) fn host_only_services(&self) -> Vec<String> {
        let st = self.st();
        st.contracts
            .iter()
            .filter(|(_, contract)| {
                contract.owner.is_none()
                    && !contract.methods.is_empty()
                    && contract.methods.values().all(super::contracts::MethodSpec::is_host)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The services plugin `plugin` called `ctx:provide` for since its latest apply, including
    /// ones refused for not being declared in `provide`.
    pub fn provided(&self, plugin: &str) -> Vec<String> {
        self.st().provided.get(plugin).cloned().unwrap_or_default()
    }

    pub fn service(&self, name: &str) -> Option<Value> {
        let found = self.st().live_service(name).map(|e| e.value.clone());
        found.or_else(|| self.cordis_service(name))
    }

    /// A service the cordis tree provides, once boot.lua ran.
    fn cordis_service(&self, name: &str) -> Option<Value> {
        let booted = self.lua().named_registry_value::<mlua::Table>("mare.cordis_boot").ok()?;
        let lookup: mlua::Function = booted.get("service").ok()?;
        match lookup.call::<Value>(name).ok()? {
            Value::Nil => None,
            v => Some(v),
        }
    }

    /// The currently selected service provider, for diagnostics. This identifies who supplied
    /// the service; it does not assert that the provider enforces a particular policy.
    pub fn service_provider(&self, name: &str) -> Option<(String, Origin)> {
        let st = self.st();
        let service = st.live_service(name)?;
        let ctx = st.ctxs.get(&service.ctx)?;
        let def = st.defs.iter().find(|def| def.name == ctx.plugin)?;
        Some((ctx.plugin.clone(), def.source.origin))
    }
}

impl crate::State {
    /// The provider of `name`, unless it is being withdrawn (its owner is disposing).
    pub(crate) fn live_service(&self, name: &str) -> Option<&crate::ServiceEntry> {
        self.services.get(name).filter(|entry| !entry.lifetime.is_cancelled())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use mlua::{Lua, Table, Value};
    use tokio::sync::oneshot;

    use super::Kernel;

    const NAME: &str = "slot-test";
    const OPTIONS: &str = r#"return { contract = { methods = {
        echo = { doc = "echo", expose = "both", params = { type = "object",
            properties = { text = { type = "string" } }, required = { "text" }, additionalProperties = false },
            order = { "text" }, result = { type = "string" } } } } }"#;

    fn kernel() -> Kernel {
        Kernel::new(Lua::new(), serde_json::json!({})).unwrap()
    }

    #[tokio::test]
    async fn changing_event_contracts_invalidates_the_bus_quiet_cache() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let kernel = kernel();
                kernel.lua().load("mare = mare or {}; mare.task = mare.task or { clock = function() return 0 end }; mare.log = mare.log or function() end").exec().unwrap();
                kernel.cordis_boot().unwrap();
                let quiet = kernel.bus("quiet").unwrap();
                let events = ["widget/activity-started", "widget/activity-completed"];
                for event in events {
                    assert!(quiet.call::<bool>(event).unwrap(), "{event} initially has no tap");
                }
                let source = crate::PluginSource {
                    name_hint: "widget-probe".into(),
                    origin: crate::Origin::User,
                    path: None,
                    code: Some(br#"return { name = 'widget-probe', contracts = { widget = { methods = {}, events = {
                        ['activity-started'] = { doc = 'start', expose = 'both', delivery = 'lossy', ignorable = false,
                          retain = { key = 'item_id', cleared_by = 'widget/activity-completed' },
                          data = { type = 'object', properties = { item_id = { type = 'string' } }, required = { 'item_id' } },
                          example = { item_id = 'a' } },
                        ['activity-completed'] = { doc = 'end', expose = 'lua', delivery = 'lossy', ignorable = true,
                          data = { type = 'object', properties = { item_id = { type = 'string' } }, required = { 'item_id' } },
                          example = { item_id = 'a' } },
                    } } }, apply = function() end }"#),
                };
                let definition = kernel.load_def(&source).unwrap();
                assert!(kernel.merge_defs(vec![definition]).1.is_empty());
                for event in events {
                    assert!(!quiet.call::<bool>(event).unwrap(), "{event} must be tapped after loading the contract");
                }
                assert!(kernel.set_defs(Vec::new()).is_empty());
                for event in events {
                    assert!(quiet.call::<bool>(event).unwrap(), "{event} must be quiet after removing the contract");
                }
            })
            .await;
    }

    fn provide(kernel: &Kernel, owner: u64, marker: &str) -> mlua::Result<u64> {
        let value: Table = kernel.lua().load("return { echo = function(_, text) return text end }").eval()?;
        value.set("marker", marker)?;
        let value = Value::Table(value);
        let options: Table = kernel.lua().load(OPTIONS).eval()?;
        let admitted = kernel.admit_service(owner, NAME, &value, Some(options), false)?;
        kernel.register_admitted(owner, NAME, value, admitted)
    }

    fn marker(kernel: &Kernel) -> Option<String> {
        match kernel.service(NAME) {
            Some(Value::Table(value)) => Some(value.get("marker").unwrap()),
            _ => None,
        }
    }

    // Hold a real registration release inside dispose_ctx, after its services are cancelled but
    // before the owner and its services are removed.
    fn hold_disposal(kernel: &Kernel, owner: u64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let entered = Rc::new(RefCell::new(Some(entered_tx)));
        let release = Rc::new(RefCell::new(Some(release_rx)));
        let disposer = kernel
            .lua()
            .create_async_function(move |_, ()| {
                let entered = entered.borrow_mut().take().unwrap();
                let release = release.borrow_mut().take().unwrap();
                async move {
                    entered.send(()).unwrap();
                    release.await.unwrap();
                    Ok(())
                }
            })
            .unwrap();
        kernel
            .st()
            .ctxs
            .get_mut(&owner)
            .unwrap()
            .registrations
            .insert("held".into(), Rc::new(tokio::sync::Mutex::new(Some(disposer))));
        (entered_rx, release_tx)
    }

    #[test]
    fn a_second_live_provider_is_refused_and_the_first_stays() {
        let kernel = kernel();
        let first = kernel.open_owner("first", "user").unwrap();
        let second = kernel.open_owner("second", "user").unwrap();
        provide(&kernel, first, "first").unwrap();

        let error = provide(&kernel, second, "second").unwrap_err();
        assert!(error.to_string().contains("service `slot-test` is already provided"), "{error}");
        assert_eq!(crate::dispatch::service_error::ServiceCallError::from(error).code(), "already_provided");
        assert_eq!(marker(&kernel).as_deref(), Some("first"));
    }

    #[test]
    fn a_closed_owners_leftover_provider_frees_the_name() {
        let kernel = kernel();
        let first = kernel.open_owner("first", "user").unwrap();
        let second = kernel.open_owner("second", "user").unwrap();
        provide(&kernel, first, "first").unwrap();
        // its fiber closed the owner without withdrawing the provider first
        kernel.owner_close(first);
        assert!(marker(&kernel).is_none());
        provide(&kernel, second, "second").unwrap();
        assert_eq!(marker(&kernel).as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn replacement_during_disposal_survives_the_old_owners_removal() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let kernel = kernel();
                let first = kernel.open_owner("first", "user").unwrap();
                let second = kernel.open_owner("second", "user").unwrap();
                let old_seq = provide(&kernel, first, "first").unwrap();
                let (entered, release) = hold_disposal(&kernel, first);
                let disposing = kernel.dispose_ctx(first);
                tokio::pin!(disposing);
                tokio::select! {
                    _ = &mut disposing => panic!("disposal returned before its registration was released"),
                    result = tokio::time::timeout(std::time::Duration::from_secs(5), entered) => {
                        result.expect("registration release did not start").unwrap()
                    },
                }

                let new_seq = provide(&kernel, second, "second").unwrap();
                assert_eq!(marker(&kernel).as_deref(), Some("second"));
                assert!(!kernel.withdraw_now(first, new_seq));
                assert!(!kernel.withdraw_now(second, old_seq));
                release.send(()).unwrap();
                disposing.await;
                assert_eq!(marker(&kernel).as_deref(), Some("second"));
                assert!(!kernel.withdraw_now(first, old_seq));
                assert!(kernel.withdraw_now(second, new_seq));
                assert!(marker(&kernel).is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn readers_ignore_a_cancelled_provider_during_disposal() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let kernel = kernel();
                let owner = kernel.open_owner("first", "user").unwrap();
                let caller = kernel.open_owner("caller", "user").unwrap();
                kernel.set_caller_access(
                    caller,
                    Some(crate::CallerAccess {
                        is_peer: true,
                        allows: Rc::new(|_, _, _| false),
                        allows_session_path: Rc::new(|| false),
                    }),
                );
                provide(&kernel, owner, "first").unwrap();
                assert!(kernel.declared_contract(NAME).is_some());
                assert!(kernel.has_service_exports());
                assert!(!kernel.service_exports().is_empty());
                assert!(kernel.service_method(NAME, "echo").is_some());
                assert!(!kernel.permitted_for_caller(Some(caller), NAME, "missing"));
                let (entered, release) = hold_disposal(&kernel, owner);
                let disposing = kernel.dispose_ctx(owner);
                tokio::pin!(disposing);
                tokio::select! {
                    _ = &mut disposing => panic!("disposal returned before its registration was released"),
                    result = tokio::time::timeout(std::time::Duration::from_secs(5), entered) => {
                        result.expect("registration release did not start").unwrap()
                    },
                }

                assert!(kernel.service(NAME).is_none());
                assert!(kernel.declared_contract(NAME).is_none());
                assert!(!kernel.has_service_exports());
                assert!(kernel.service_exports().is_empty());
                assert!(kernel.service_method(NAME, "echo").is_none());
                assert!(kernel.permitted_for_caller(Some(caller), NAME, "missing"));
                assert!(kernel.validate_result(NAME, "echo", Value::Nil).unwrap_err().contains("unavailable"));
                let error = kernel
                    .call_service_export(caller, None, NAME, "echo", serde_json::json!({}), Default::default())
                    .await
                    .unwrap_err();
                assert!(matches!(error, crate::ServiceCallError::Unavailable));
                release.send(()).unwrap();
                disposing.await;
            })
            .await;
    }

    #[test]
    fn host_provider_methods_are_reserved_and_not_overridable_by_plugins() {
        let kernel = kernel();
        // Third-party plugin cannot declare provider = "host"
        let third_party = crate::PluginSource {
            name_hint: "third-host".into(),
            origin: crate::Origin::User,
            path: None,
            code: Some(
                br#"return {
                    name = "third-host",
                    contracts = {
                        custom = {
                            methods = {
                                ping = { doc = "host ping", expose = "lua", provider = "host", result = { type = "boolean" } },
                            },
                        },
                    },
                    apply = function() end,
                }"#,
            ),
        };
        let err = kernel.load_def(&third_party).err().expect("plugin contract with provider=host must fail");
        assert!(err.to_string().contains("host"), "{err}");

        // Runtime options.contract cannot declare provider = "host"
        let owner = kernel.open_owner("rt-probe", "user").unwrap();
        let runtime_opts: Table = kernel
            .lua()
            .load(
                r#"return { contract = { methods = {
                    ping = { doc = "host ping", expose = "lua", provider = "host", result = { type = "boolean" } },
                } } }"#,
            )
            .eval()
            .unwrap();
        let impl_val: Value = kernel.lua().load("return {}").eval().unwrap();
        let err = kernel
            .admit_service(owner, "rt-host", &impl_val, Some(runtime_opts), false)
            .err()
            .expect("runtime contract with provider=host must fail")
            .to_string();
        assert!(err.contains("host"), "{err}");

        // Built-in installed contract with mixed host + plugin methods:
        let builtin_contracts: Table = kernel
            .lua()
            .load(
                r#"return {
                    hybrid = {
                        methods = {
                            host_op = { doc = "host op", expose = "lua", provider = "host", result = { type = "boolean" } },
                            plugin_op = { doc = "plugin op", expose = "lua", result = { type = "boolean" } },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        kernel.install_service_contracts(builtin_contracts).unwrap();

        // Implementing only plugin_op is admitted
        let ok_impl: Value = kernel.lua().load("return { plugin_op = function() return true end }").eval().unwrap();
        kernel.admit_service(owner, "hybrid", &ok_impl, None, false).expect("plugin omitting host_op must be admitted");

        // Attempting to implement/override host_op is rejected with invalid_arguments
        let bad_impl: Value = kernel
            .lua()
            .load("return { plugin_op = function() return true end, host_op = function() return false end }")
            .eval()
            .unwrap();
        let err = kernel
            .admit_service(owner, "hybrid", &bad_impl, None, false)
            .err()
            .expect("plugin implementing host_op must be rejected");
        let coded = match &err {
            mlua::Error::ExternalError(arc) => arc.downcast_ref::<mare_shared::errors::Coded>(),
            _ => None,
        };
        assert_eq!(coded.map(|c| c.code), Some("invalid_arguments"), "{err}");
    }
}
