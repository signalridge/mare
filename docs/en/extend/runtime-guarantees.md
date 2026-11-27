# Plugin runtime guarantees

What mare promises a plugin author about lifecycle, services, registrations, reloads and cleanup. A plugin can rely on each of these; a change that breaks one is a breaking change. Behaviour not listed here is still documented in [Plugins](plugins.md) and [Services and events](services-and-events.md), but it is not part of this list.

Each promise has a key. To find the tests for a promise, search for its key: `rg 'guarantee: effects-dispose-in-reverse-start-order'`.

## Dependencies and waiting

| Key | Promise |
|---|---|
| `inject-waits-instead-of-failing` | A plugin whose `inject` names a service nobody provides is not a failure: it waits (PENDING), the host reports what it waits for, and it starts as soon as a provider appears. |
| `inject-cycle-refused-and-the-rest-starts` | Plugins that inject services only each other provide are refused before they run; the problem names the cycle, and every plugin outside the cycle starts normally. |
| `dependents-stop-before-their-provider-and-restart-on-the-new-one` | When a provider is reloaded or replaced, the plugins that inject its service are disposed before it and started again on the new provider, never left on the old one. |
| `dependent-waits-while-its-provider-is-gone` | When a provider goes away, its dependents are disposed (their cleanups run) and wait; that is not a failure, and they start again when a provider comes back. |
| `provider-outlives-dependents-in-flight-cleanup` | A provider's disposal waits for the cleanups its dependents are still running, so a dependent's cleanup can still call the service it was started on. |

## Services

| Key | Promise |
|---|---|
| `one-provider-per-service-name` | A service name has one provider. A second definition that declares the same name in `provide` is refused before it runs, the error names both files, and the first keeps the service. |
| `no-fallback-when-a-provider-leaves` | A provide into a taken name at run time is refused with `already_provided` and the holder keeps answering; when the holder goes away nothing falls back, and the name has no provider until a plugin provides it again. |
| `undeclared-raw-provide-refused` | A declared service is provided only through `ctx:plugin` with a `mare.Service` class, so its contract checks every call; a raw `ctx:provide` under a declared name is refused. |
| `host-methods-stay-with-the-host` | A method the host implements in a service contract cannot be implemented or replaced by a plugin. |

## Ownership of registrations

| Key | Promise |
|---|---|
| `ctx-registrations-withdrawn-on-dispose` | Everything registered through a plugin's `ctx` (listeners, services, timers, tasks, tools, commands, key bindings, status segments, effects) is withdrawn when the plugin is disposed. |
| `registration-withdrawn-on-caller-unload` | A registration made through another plugin's service belongs to the caller: it is withdrawn when the caller goes away, its release is idempotent, and an explicit release is not released again. |
| `release-after-provider-is-gone-does-nothing` | When the provider went away first, the release its former caller still holds does nothing and never calls into the unloaded provider. |
| `no-registration-escapes-an-unload` | A registration made from a cleanup while the plugin unloads does not outlive the plugin: it never runs afterwards. |
| `shared-cleanup-runs-once-per-registration` | One cleanup function registered by two plugins is two registrations: disposing one plugin runs it once and leaves the other's in place. |

## Cleanup

| Key | Promise |
|---|---|
| `effects-dispose-in-reverse-start-order` | A plugin's cleanups start last-in first-out, each exactly once. Put cleanups whose order matters into one effect. |
| `cleanup-error-does-not-stop-the-rest` | A cleanup that raises does not stop the others; its error is reported once. |
| `yielding-cleanups-run-concurrently` | Cleanups that yield (wait on a timer, a process, the network) run concurrently: one does not wait for another to finish, and the unload ends when all of them have. |
| `turning-a-plugin-off-waits-for-its-cleanup` | Turning a plugin off waits for its cleanup, asynchronous parts included, before the reload reports done. |

## Failures and rollback

| Key | Promise |
|---|---|
| `failed-apply-rolled-back-before-it-is-reported` | When `apply` raises, what it provided and registered is gone and its cleanups have run before the failure is reported. |
| `failed-reload-keeps-the-old-definition-serving` | When a reload's new definition fails in `apply`, the previous definition runs again as a fresh instance (its earlier in-memory state is not restored), its dependents are applied again, and the failure is reported. A full `/reload` keeps such a plugin running. |
| `broken-definition-reload-changes-nothing` | A reload whose new definition cannot be read or fails validation before `apply` leaves the running instance untouched. |
| `dependents-put-back-with-their-provider` | When a provider and its dependents all fail on their new definitions in one reload, each is put back on its previous definition, dependents after their provider. |
| `handler-error-reported-and-plugin-keeps-running` | A listener, task or timer that raises is reported every time (`kernel/error`, a notice) and the plugin stays loaded; its latest error stays in the problems until a full `/reload`. |

## Reload

| Key | Promise |
|---|---|
| `config-change-restarts-only-its-plugin` | Changing one plugin's options restarts that plugin only. |
| `rejected-config-keeps-the-previous-one` | A plugin config that its schema rejects keeps the previous config running; a corrected edit applies. |
| `cancelled-reload-waits-for-disposal` | A reload cancelled while a cleanup is running does not leave a half-disposed plugin: retrying waits for that disposal first. |
| `full-reload-keeps-startup-and-session` | A full `/reload` restarts every plugin without replaying startup (no second `app/ready`, no second `session/start`) and keeps the session the user is in. |
| `reload-rereads-plugin-modules` | Reloading a plugin drops the modules it `require`d, so edited module files are read again. |

## Shutdown and stuck plugins

| Key | Promise |
|---|---|
| `shutdown-is-bounded-and-names-unfinished-cleanups` | Exit waits for plugin cleanups up to `shutdown_grace_ms`. Past the deadline it stops waiting, and names the plugins still unloading, also those whose unload began before the exit; the other plugins have unloaded. |
| `unload-stall-reported-with-effect-labels` | A plugin still unloading after `stall_ms` (default 10 s, 0 turns it off) is reported with the labels (`ctx:effect(fn, label)`) of the effects it held when its dispose began, once per stall, in `/plugins tree`, `mare plugin tree --json`, the log and a `kernel/stalled` notice. The labels are a snapshot taken when the unload starts: they name every cleanup that started, not only the one still running. A restart caused by a config change does not go through dispose, so its report carries no labels. |
| `load-stall-reported-without-abort` | A plugin whose `apply` has not returned after `stall_ms` is reported the same way; nothing is aborted or isolated, and the reload that started it keeps waiting. |
| `pending-stall-reported-as-waiting` | A plugin left waiting for a service after `stall_ms` is reported with the services it waits for. It stays PENDING: the report adds no problem line and never turns the wait into a failure. |
