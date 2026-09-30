# quantum-plugin-file-renderer

The **first external plugin** for the quantum notes app (design.md **D25**,
[`docs/external-plugins.md`](https://github.com/chaosbolt99999/quantum/blob/main/docs/external-plugins.md)
§2–§4, §8).

It is a dependency-light Rust `cdylib` that renders `file@1` blocks: images inline, video as
a card, markdown as rendered text, and PDFs — plus everything else — as a document card.

Its **only** interface with the app is the §3 JSON ABI. It does not depend on the quantum
workspace, the kernel, gpui, or any other crate: `serde` and `serde_json` are the entire
dependency list, and the whole thing is one file, [`src/lib.rs`](src/lib.rs).

## The ABI (§3)

```c
uint32_t    quantum_plugin_v1_abi(void);                       /* returns 1              */
const char* quantum_plugin_v1_describe(void);                  /* static JSON descriptor */
char*       quantum_plugin_v1_render(const char* request_json); /* owned JSON response   */
void        quantum_plugin_v1_free(char* ptr);                 /* frees a response      */
```

* **`describe()`** returns the full descriptor object — `{"abi":1,"id":…,"version":…,
  "renderers":[…]}` — as `'static` memory the host must not free. It is the runtime mirror of
  `plugin.json`, and a unit test asserts that its `renderers` array parses equal to
  `plugin.json`'s, so the two descriptors cannot drift.
* **`render()`** takes one NUL-terminated JSON request and returns **one owned** response
  freed by `free()`. Returning an owned pointer keeps the contract thread-agnostic and
  impossible to get wrong by accident.
* **Nothing panics across the boundary.** The entire body of `render()` runs inside
  `guard()`, which is `catch_unwind`: a panic becomes an `{"error": "plugin panicked: …"}`
  response, and `[profile.release] panic = "unwind"` keeps that guarantee meaningful. A test
  drives a real panic through the raw C entry point to prove it.
* Every entry point is `extern "C"` with `#[no_mangle]`. No Rust type crosses the edge —
  only NUL-terminated UTF-8 JSON.

A response is one of exactly two shapes:

```json
{ "plan":  { "kind": "image", "source": "/abs/workspace/pic.png", "alt": "pic.png" } }
{ "error": "cannot read file at `docs/gone.md` (resolved to …): No such file or directory" }
```

## Renderers (§8)

| Renderer | Matches | Plan |
|---|---|---|
| `file-renderer.media` | `image/*`, `video/*` | `image` (inline) for images, `video` card for video |
| `file-renderer.markdown` | `text/markdown`, `text/plain` | `markdown` with the file's text, capped at 256 KiB with a visible truncation note |
| `file-renderer.document` | `application/pdf`, **`*/*`** (anything, including an empty mime) | `document` card; `pages` probed for PDFs |

Priority is 100 / 90 / 10, so media wins over the document card for an image, and markdown
wins over it for `text/plain`.

**Rendering never writes anything, and never guesses the workspace.** The request's
`workspace_root` plus the block's **relative** `path` resolve the file:

* an absolute path, or one that climbs above the root (`../x`, `docs/../../x`), is refused
  with an `error` plan **before the file is opened**;
* a symlink *inside* the root that resolves outside it is refused too (the lexical check is
  followed by a `canonicalize` containment re-check, which is what §4's re-check buys);
* a relative path that stays inside the root (`docs/../docs/spec.md`) is fine.

The `pages` count is a **heuristic byte scan**, not a parser: it counts `/Type`+whitespace+
`/Page` where the next byte is not `s` (so the page *tree* node `/Type /Pages` is not
counted). A PDF whose page objects live in a compressed object stream reports `0`, and `0`
means "unknown", never "not a PDF" — the plan then carries a `note` saying so.

## Bundle and release (§2)

```
manifest.toml    D21 static data — id/version/api-range, empty capabilities/provides/registers
plugin.json      the runtime descriptor: abi, id, version, entry, entry_sha256, renderers
lib/<entry>      the cdylib
```

A **renderer-only** plugin declares zero `[[registers]]`: `file@1` is owned by the bundled
`files` plugin, and this plugin only renders it. `registers = []` is still present, because
the kernel's manifest reader treats an absent key as a parse error while an empty list is
valid — and the host's load path must not treat an empty register list as an error.

```sh
cargo test                                   # unit tests, including the refusals
tools/bundle.sh                              # build + assemble + tar + print SHA-256
gh release create v0.1.0 dist/*.tar.gz       # publish the artifact
```

`tools/bundle.sh` builds the release library, computes the entry digest, stamps it into
`plugin.json` (both the copy inside the bundle and the committed one, so `git status` after
bundling is a drift check), tars `manifest.toml`, `plugin.json` and `lib/<entry>` at the
bundle root, verifies the artifact it just wrote, and prints:

```
TARGET=<host triple>
ENTRY=lib/libquantum_plugin_file_renderer.so
ENTRY_SHA256=<sha256 of the entry library>
BUNDLE=dist/quantum-plugin-file-renderer-0.1.0-<target>.tar.gz
BUNDLE_SHA256=<sha256 of the tarball>
```

Install with `PluginSource::Https(<the asset URL>)` plus the pinned `BUNDLE_SHA256`, or with
`PluginSource::LocalDir(<an extracted bundle>)` for development.

## Testing

`cargo test` covers each plan kind, the refusals (missing file, escape attempt by `..` and by
symlink, absolute path, directory, unknown mime, wrong block type, wrong ABI, malformed
JSON, unknown renderer, missing fields), the markdown cap and its truncation note (including
a cap that splits a multi-byte character), the PDF probe (page tree excluded, needle
straddling the 64 KiB read boundary), panic containment through the raw C entry point, the
`describe()`↔`plugin.json` agreement, and the tarball-facing shape of `manifest.toml` /
`plugin.json`.
