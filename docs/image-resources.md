# Output-backed image resources

A historical image node can retain a resource description instead of PNG bytes.
`ctx.ui:artifact_image(artifact, opts)` binds a saved spill locator and digest to
its current session epoch. The default loader reads through the same
`spill` service and validates owner before and after the read, byte size and
SHA-256. It does not parse a locator, open an arbitrary path, execute a helper or
create another persistent store.

```lua
local saved = ctx.spill:save_bytes(owner.id, png)
local node = ctx.ui:artifact_image({
  session_id = owner.id, spill_id = saved.spill_id,
  sha256 = mare.crypto.sha256(png), width = width, height = height,
}, { max_rows = 20, max_cols = 100, fallback = mare.ui.Code(source, language) })
```

The descriptor is immutable after snapshot. Nodes and layout caches retain its
locator, dimensions and registration identity. Bytes are fetched only when an
image worker requests a visible rendition. The callback runs on Lua's existing
async host thread in a fresh `ephemeral-read-only` execution; rendering and
layout measurement do not call Lua or read artifact storage. Normal renderer
execution remains a separate approved tool operation.

The native raw-byte cache has a shared 32 MiB budget and 64-entry ceiling, with
8 MiB maximum per image. The existing encode cache separately retains at most
32 encodings of at most 2 MiB each. Decoder concurrency stays at the existing two
image workers. A resolver has at most 16 queued read requests and each read has
a 30-second ceiling. Source keys have a 128 KiB ceiling; a Lua host can register
at most 64 resolver namespaces. These are image budgets, not a claim that all
ordinary conversation text, terminal-owned graphics or the process RSS fits in
32 MiB.

`ui.image_cache_info()` reports aggregate byte/entry budgets without locators.
A native resource registration has a fresh identity on every installation.
Disposal cancels pending callbacks, rejects old references and clears its raw
cache entries. The built-in output namespace also rotates registration identity
on session replacement, so even already cached bytes cannot cross an owner or
session epoch change. A changed digest, missing locator, revoked backend or failed
read renders the supplied complete source fallback.

The lower-level `ui.image_resolver(namespace, load)` returns a disposer;
registrants attach it to their owning context's disposal. `ui.image_resource`
creates a small descriptor without IO. A custom resolver must include its real
owner/session epoch in the key and enforce storage authorization. The key is data,
not permission. Native resolver identities isolate registrations; they do not
grant filesystem or tool authority. Inline `ui.image{bytes=...}` remains
compatible for callers that deliberately own those bytes. Mermaid historical
nodes use resources rather than that inline form.

## Viewports and source

Layout passes reserve known resource dimensions without decoding or queueing.
The owned Ctrl+T transcript caches layout/text rather than image protocol raw
strings. Only blocks and image rows intersecting its viewport materialize.
Images becoming ready invalidate that layout cache; committed asynchronous
updates reuse the existing debounced reflow. Resource loads never block the
Lua/UI event loop, including history replay.

Expanded Markdown retains ordinary text once. Each enhanced fence shows its
image and complete code source. Its parser offsets, quote/list context and
wrappers remain native Markdown. Native terminal scrollback provides no
visibility callbacks to the application; after reflow only its current visible
tail uploads images. Ctrl+T is the application-owned viewport for revisiting
and rematerializing older references. The persistent source and locator remain
available when byte caches evict an image.

## RPC

mare RPC protocol 3.0 includes optional `viewport.supports_artifact_images`. A presenter can declare
`requires_artifact_images=true`; an image-capable consumer that omits the new
capability receives complete source and a diagnostic, without running that
presenter. The protocol schema exports `ui_image_resource` and `ui_output_image_key`.
The resource wire shape is `{namespace,key,width,height}`. Native `resolver_id`
is omitted on encode and rejected on decode. A Lua frontend can decode only by
rebinding to a resolver namespace already registered in that host.

For namespace `output`, the JSON key fields are `session_id`, `session_epoch`,
`spill_id` and `sha256`. Keep `spill_id` opaque. Read it with `spill.read` (`encoding: "base64"`) on
the originating RPC connection, apply the same size limit and verify SHA-256.
That existing method enforces the connection's actual session owner and output
backend namespace; resource metadata grants no additional access. A remote
frontend must explicitly implement this contract rather than infer support
from the protocol version alone.

## Design rationale

The resource model keeps original fenced source readable, separates layout
measurement from the raster and protocol-encoding caches, and stores only
metadata plus opaque `spill` locators on historical nodes so evicted images can
rematerialize on demand in the `Ctrl+T` viewport. Callback ownership comes from
the existing scoped UI origin primitive, without introducing a second UI loop
or upload queue.
