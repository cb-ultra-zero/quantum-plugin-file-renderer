//! # quantum-plugin-file-renderer
//!
//! The **first external plugin** for the quantum notes app (design.md D25,
//! `docs/external-plugins.md`): three `file@1` renderers — `file-renderer.media`,
//! `file-renderer.markdown` and `file-renderer.document` — behind that document's §3
//! C ABI.
//!
//! The plugin's **only** interface with the app is §2/§3/§4 of that document: a
//! NUL-terminated JSON request in, an owned NUL-terminated JSON response out. There is no
//! dependency on the quantum workspace, the kernel, gpui, or anything platform-specific —
//! `serde` and `serde_json` are the whole dependency list — so this library keeps
//! compiling and loading against any host that speaks ABI v1.
//!
//! Invariants worth stating in one place:
//!
//! * [`quantum_plugin_v1_abi`] returns [`PLUGIN_ABI_VERSION`] (`1`).
//! * [`quantum_plugin_v1_describe`] returns a **static**, NUL-terminated *full descriptor
//!   object* (`{"abi":1,"id":…,"version":…,"renderers":[…]}`) whose `renderers` array
//!   parses equal to the `renderers` array of `plugin.json`; a test here enforces that, so
//!   the runtime descriptor and the install descriptor cannot drift.
//! * [`quantum_plugin_v1_render`] never panics across the boundary: its whole body runs
//!   inside [`guard`] (`catch_unwind`), so a panic becomes an `{"error": …}` response
//!   instead of undefined behaviour across the FFI edge (§3). A test drives a real panic
//!   through the raw entry point to prove it.
//! * The plugin **reads** files below the request's `workspace_root` and never writes
//!   anything (§8). A `path` that escapes the root is refused with an `error` plan, and the
//!   file is not read.
//!
//! The response is one of exactly two shapes (§3):
//!
//! ```json
//! { "plan":  { "kind": "image", "source": "/abs/workspace/pic.png", "alt": "pic.png" } }
//! { "error": "cannot read file at `docs/gone.md` (resolved to …): No such file or directory" }
//! ```

use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------------------

/// The one ABI this plugin speaks (`docs/external-plugins.md` §7: `PLUGIN_ABI_VERSION = 1`;
/// a bundle declaring another version is refused at install).
pub const PLUGIN_ABI_VERSION: u32 = 1;

/// The plugin id, as it appears in both `manifest.toml` and `plugin.json`.
pub const PLUGIN_ID: &str = "file-renderer";

/// The plugin version, as it appears in both `manifest.toml` and `plugin.json`.
pub const PLUGIN_VERSION: &str = "0.1.0";

/// The block type these renderers are for. `file@1` is owned by the bundled `files`
/// plugin; this plugin only *renders* it (§2, §8).
pub const FILE_TYPE_ID: &str = "file@1";

/// Renderer ids — the values the host puts in a request's `renderer` field and matches
/// against `plugin.json`.
pub const RENDERER_MEDIA: &str = "file-renderer.media";
/// See [`RENDERER_MEDIA`].
pub const RENDERER_MARKDOWN: &str = "file-renderer.markdown";
/// See [`RENDERER_MEDIA`].
pub const RENDERER_DOCUMENT: &str = "file-renderer.document";

/// The mime used when a `file@1` block has no `mime` attr but lands on the catch-all
/// [`RENDERER_DOCUMENT`].
pub const DEFAULT_MIME: &str = "application/octet-stream";

/// The text cap for [`RENDERER_MARKDOWN`] (256 KiB). A larger file is truncated with a
/// visible note; a plan is ephemera and must never make the host hold a whole corpus.
pub const MAX_TEXT_BYTES: u64 = 256 * 1024;

/// The PDF page probe stops after this many bytes (64 MiB). The probe is a heuristic byte
/// scan, not a parser, so scanning a multi-gigabyte file end to end would buy nothing.
pub const MAX_PDF_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes read per probe chunk.
const PDF_CHUNK: usize = 64 * 1024;

/// `renderer` value that exists only to prove panic containment from a test. It is
/// compiled out of release builds.
#[cfg(test)]
const TEST_PANIC_RENDERER: &str = "file-renderer.__test_panic";

/// The static descriptor handed out by [`quantum_plugin_v1_describe`] — the full descriptor
/// object, NUL-terminated (§3). Kept in sync with `plugin.json` by
/// `describe_matches_plugin_json_renderers`.
///
/// It is a `concat!` of literals so the pointer handed across the FFI edge is `'static`
/// and never allocated.
const DESCRIPTOR_CSTR: &str = concat!(
    r#"{"abi":1,"id":"file-renderer","version":"0.1.0","renderers":["#,
    r#"{"id":"file-renderer.media","label":"Media","types":["file@1"],"mimes":["image/*","video/*"],"priority":100},"#,
    r#"{"id":"file-renderer.markdown","label":"Markdown","types":["file@1"],"mimes":["text/markdown","text/plain"],"priority":90},"#,
    r#"{"id":"file-renderer.document","label":"Document","types":["file@1"],"mimes":["application/pdf","*/*"],"priority":10}"#,
    r#"]}"#,
    "\0",
);

/// The descriptor without its trailing NUL — the same bytes the host receives.
pub fn descriptor_json() -> &'static str {
    DESCRIPTOR_CSTR
        .strip_suffix('\0')
        .unwrap_or(DESCRIPTOR_CSTR)
}

// ---------------------------------------------------------------------------------------
// The C ABI (§3)
// ---------------------------------------------------------------------------------------

/// `uint32_t quantum_plugin_v1_abi(void)` — must return 1.
#[no_mangle]
pub extern "C" fn quantum_plugin_v1_abi() -> u32 {
    PLUGIN_ABI_VERSION
}

/// `const char* quantum_plugin_v1_describe(void)` — a static, NUL-terminated JSON
/// descriptor; the host never frees it and cross-checks it against `plugin.json`.
#[no_mangle]
pub extern "C" fn quantum_plugin_v1_describe() -> *const std::os::raw::c_char {
    DESCRIPTOR_CSTR.as_ptr().cast()
}

/// `char* quantum_plugin_v1_render(const char* request_json)` — one owned, NUL-terminated
/// JSON response, freed by [`quantum_plugin_v1_free`].
///
/// The entire body runs inside [`guard`], so no panic can cross the boundary (§3).
///
/// `unsafe` here is **Rust-side metadata only**: the C symbol is exactly
/// `char* quantum_plugin_v1_render(const char*)`, unchanged, and the host reaches it through
/// `dlsym`, which never sees this marker. It exists so a *Rust* caller cannot hand this
/// function a dangling or non-NUL-terminated pointer without saying so.
///
/// # Safety
///
/// `request_json` is either null (which is answered with an error response) or a pointer to
/// a NUL-terminated UTF-8 string that stays alive for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn quantum_plugin_v1_render(
    request_json: *const std::os::raw::c_char,
) -> *mut std::os::raw::c_char {
    let response = guard(|| {
        if request_json.is_null() {
            return error_response("request pointer is null");
        }
        // SAFETY: §3 — the host passes a NUL-terminated UTF-8 string that stays alive for
        // the duration of the call. That is the only precondition we rely on.
        let request = unsafe { CStr::from_ptr(request_json) };
        match request.to_str() {
            Ok(json) => render_json(json),
            Err(e) => error_response(&format!("request is not valid UTF-8: {e}")),
        }
    });
    into_owned_c_string(response)
}

/// `void quantum_plugin_v1_free(char* ptr)` — frees a response from
/// [`quantum_plugin_v1_render`]; a null pointer is a no-op.
///
/// `unsafe` is Rust-side metadata only (see [`quantum_plugin_v1_render`]): the C symbol is
/// unchanged.
///
/// # Safety
///
/// `ptr` is null or a pointer returned by [`quantum_plugin_v1_render`] that has not already
/// been freed.
#[no_mangle]
pub unsafe extern "C" fn quantum_plugin_v1_free(ptr: *mut std::os::raw::c_char) {
    if ptr.is_null() {
        return;
    }
    // The guard only keeps a panic (e.g. a poisoned allocator) from crossing the boundary;
    // a double free is undefined behaviour by contract and no guard can rescue it.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `ptr` came from `CString::into_raw` in `into_owned_c_string` and the host
        // calls `free` at most once per response (§3).
        drop(unsafe { CString::from_raw(ptr) });
    }));
}

// ---------------------------------------------------------------------------------------
// Panic containment
// ---------------------------------------------------------------------------------------

/// Run `f`, turning a panic into an `{"error": …}` response.
///
/// This is the wrapper every FFI entry point uses, and it is public so tests (and any
/// future entry point) exercise the same code path that production runs (§3: a plugin must
/// not panic across the boundary; the host reports a panicking plugin as a runtime error
/// and disables that renderer for the session).
pub fn guard<F>(f: F) -> String
where
    F: FnOnce() -> String,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(response) => response,
        Err(payload) => error_response(&format!("plugin panicked: {}", panic_message(&payload))),
    }
}

/// Best-effort text for a panic payload (`&str` / `String` are the two standard shapes).
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// Move a response onto the C heap. The JSON never contains an interior NUL; if that ever
/// changes we return an error response rather than panicking.
fn into_owned_c_string(response: String) -> *mut std::os::raw::c_char {
    let text = if response.as_bytes().contains(&0) {
        error_response("response encoding error: interior NUL")
    } else {
        response
    };
    match CString::new(text) {
        Ok(c) => c.into_raw(),
        Err(_) => CString::new(error_response("response encoding error"))
            .expect("static JSON has no interior NUL")
            .into_raw(),
    }
}

// ---------------------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------------------

/// One §3 request. Every field is optional except that a usable request needs a
/// `renderer`, a `workspace_root` and a `block`; a missing one is a named `error` response.
/// Unknown fields are ignored on purpose: forward compatibility is the host's business, and
/// refusing an unknown key would break a newer host.
#[derive(Debug, Deserialize)]
struct Request {
    abi: Option<u32>,
    renderer: Option<String>,
    #[allow(dead_code)]
    note_id: Option<String>,
    workspace_root: Option<String>,
    block: Option<Block>,
}

/// The §3 `block` object and the `file@1` attrs this plugin reads.
#[derive(Debug, Deserialize)]
struct Block {
    #[allow(dead_code)]
    id: Option<String>,
    type_id: Option<String>,
    #[allow(dead_code)]
    text: Option<String>,
    #[serde(default)]
    attrs: Map<String, Value>,
}

impl Block {
    /// Read one attr as text. The document's attrs are strings (`"size": "8123"`), but a
    /// number or bool is coerced rather than refused — the op log's attrs are
    /// self-describing, so an integer `size` must not break rendering (§6).
    fn attr(&self, key: &str) -> Option<String> {
        match self.attrs.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// The display name: the `name` attr when non-empty, else `"file"`. (`plugin.json`'s
    /// plan vocabulary takes the name from the host's attrs; this is only a fallback.)
    fn name(&self) -> String {
        self.attr("name")
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "file".to_string())
    }
}

/// Render one request JSON string to one response JSON string. Never panics on a
/// well-formed string, and [`guard`] covers the rest.
pub fn render_json(request_json: &str) -> String {
    match render_plan(request_json) {
        Ok(plan) => json!({ "plan": plan }).to_string(),
        Err(message) => error_response(&message),
    }
}

/// The whole decision: request → plan, or the message of an `error` response.
fn render_plan(request_json: &str) -> Result<Value, String> {
    let request: Request =
        serde_json::from_str(request_json).map_err(|e| format!("malformed request JSON: {e}"))?;

    if let Some(abi) = request.abi {
        if abi != PLUGIN_ABI_VERSION {
            return Err(format!(
                "unsupported request abi {abi}: this plugin speaks {PLUGIN_ABI_VERSION}"
            ));
        }
    }

    let renderer = request
        .renderer
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| "request is missing `renderer`".to_string())?;

    let block = request
        .block
        .ok_or_else(|| "request is missing `block`".to_string())?;

    if let Some(type_id) = block.type_id.as_deref() {
        if type_id != FILE_TYPE_ID {
            return Err(format!(
                "this plugin only renders `{FILE_TYPE_ID}` blocks, not `{type_id}`"
            ));
        }
    }

    #[cfg(test)]
    if renderer == TEST_PANIC_RENDERER {
        panic!("test panic probe");
    }

    let root = request
        .workspace_root
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| "request is missing `workspace_root`".to_string())?;
    let root = Path::new(root);

    let path = block.attr("path").unwrap_or_default();
    let mime = block.attr("mime").unwrap_or_default();
    let name = block.name();

    match renderer {
        RENDERER_MEDIA => media_plan(root, &path, &name, &mime),
        RENDERER_MARKDOWN => markdown_plan(root, &path, &name, &mime),
        RENDERER_DOCUMENT => document_plan(root, &path, &name, &mime),
        other => Err(format!(
            "unknown renderer `{other}` (this plugin provides {RENDERER_MEDIA}, \
             {RENDERER_MARKDOWN}, {RENDERER_DOCUMENT})"
        )),
    }
}

fn error_response(message: &str) -> String {
    json!({ "error": message }).to_string()
}

/// `""` reads badly in an error message; name it.
fn display_mime(mime: &str) -> String {
    if mime.trim().is_empty() {
        "(none)".to_string()
    } else {
        mime.to_string()
    }
}

// ---------------------------------------------------------------------------------------
// Path resolution — the §4 containment re-check
// ---------------------------------------------------------------------------------------

/// Resolve a `file@1` block's **relative** `path` against the request's `workspace_root`
/// (design.md D-l: the op log stores the relative path, the device-local root resolves it).
///
/// Refused with a message (which becomes an `error` plan) when the path is absolute, when
/// it climbs above the root, or — the part lexical normalisation cannot see — when a
/// symlink inside the root resolves outside it. A refused path is never opened (§4: the
/// host re-checks containment, and a plugin cannot widen the app's file access by asking
/// for it).
fn resolve_below_root(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("the block has no `path` attribute to resolve".to_string());
    }

    // Component-wise, so `docs/../spec.md` stays inside the root while `../spec.md` and
    // `docs/../../spec.md` do not.
    let mut stack: Vec<OsString> = Vec::new();
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(part) => stack.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return Err(format!("path `{rel}` escapes the workspace root"));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "path `{rel}` must be relative to the workspace root"
                ));
            }
        }
    }
    if stack.is_empty() {
        return Err(format!("path `{rel}` does not name a file"));
    }

    let mut candidate = PathBuf::from(root);
    for part in stack {
        candidate.push(part);
    }

    // Symlink guard. Both sides must exist for `canonicalize` to succeed; when they do,
    // the resolved target must still be under the resolved root.
    if let (Ok(root_resolved), Ok(target_resolved)) =
        (root.canonicalize(), candidate.canonicalize())
    {
        if !target_resolved.starts_with(&root_resolved) {
            return Err(format!("path `{rel}` escapes the workspace root"));
        }
    }

    Ok(candidate)
}

/// Resolve *and* stat a path, returning it with the absolute `source` string a plan needs.
/// A missing path or a directory is refused before anything is read.
fn existing_file(root: &Path, rel: &str) -> Result<(PathBuf, String), String> {
    let path = resolve_below_root(root, rel)?;
    let metadata = std::fs::metadata(&path).map_err(|e| {
        format!(
            "cannot read file at `{rel}` (resolved to `{}`): {e}",
            path.display()
        )
    })?;
    if metadata.is_dir() {
        return Err(format!(
            "`{rel}` (resolved to `{}`) is a directory, not a file",
            path.display()
        ));
    }
    let source = path.to_string_lossy().into_owned();
    Ok((path, source))
}

// ---------------------------------------------------------------------------------------
// The three renderers (§8)
// ---------------------------------------------------------------------------------------

/// `file-renderer.media` — `image/*` becomes an inline `image` plan, `video/*` a `video`
/// card (§4: no in-app player yet, so the host draws metadata plus **Open externally**).
fn media_plan(root: &Path, rel: &str, name: &str, mime: &str) -> Result<Value, String> {
    let kind = if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("video/") {
        "video"
    } else {
        return Err(format!(
            "renderer `{RENDERER_MEDIA}` handles image/* and video/*, not `{}`",
            display_mime(mime)
        ));
    };
    let (_path, source) = existing_file(root, rel)?;
    Ok(match kind {
        "image" => json!({ "kind": "image", "source": source, "alt": name }),
        _ => json!({ "kind": "video", "source": source, "caption": name }),
    })
}

/// `file-renderer.markdown` — `text/markdown` and `text/plain` become a `markdown` plan
/// carrying the file's text, size-capped with a visible truncation note (§4 renders the
/// markdown, not the raw source).
fn markdown_plan(root: &Path, rel: &str, name: &str, mime: &str) -> Result<Value, String> {
    if !matches!(mime, "text/markdown" | "text/plain") {
        return Err(format!(
            "renderer `{RENDERER_MARKDOWN}` handles text/markdown and text/plain, not `{}`",
            display_mime(mime)
        ));
    }
    let (path, _source) = existing_file(root, rel)?;
    let (mut text, truncated, total) = read_text_capped(&path, MAX_TEXT_BYTES)
        .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    if truncated {
        let shown = text.len();
        text.push_str(&truncation_note(shown, total));
    }
    Ok(json!({ "kind": "markdown", "text": text, "title": name }))
}

/// `file-renderer.document` — `application/pdf` and **everything else** become a
/// `document` card (§4: metadata plus **Open externally**; §7 records that the host has no
/// rasteriser yet). PDFs also get a cheap `pages` probe.
fn document_plan(root: &Path, rel: &str, name: &str, mime: &str) -> Result<Value, String> {
    let (path, source) = existing_file(root, rel)?;
    let mime = if mime.trim().is_empty() {
        DEFAULT_MIME
    } else {
        mime
    };
    let mut plan = json!({ "kind": "document", "source": source, "mime": mime, "title": name });
    if mime.eq_ignore_ascii_case("application/pdf") {
        match pdf_probe(&path) {
            Ok(probe) if probe.pages > 0 => plan["pages"] = json!(probe.pages),
            Ok(probe) if !probe.has_header => {
                plan["note"] =
                    json!("page count unavailable: the file does not start with the %PDF- header")
            }
            Ok(probe) => {
                plan["note"] = json!(format!(
                    "page count unavailable: the heuristic byte scan of {} byte(s) found no \
                     page objects",
                    probe.scanned
                ))
            }
            Err(e) => plan["note"] = json!(format!("page count unavailable: {e}")),
        }
    }
    Ok(plan)
}

/// The note appended to a truncated markdown plan. It is deliberately visible text: the
/// host renders the markdown, so a silent cap would look like a lost file.
fn truncation_note(shown: usize, total: u64) -> String {
    format!("\n\n> **[file-renderer]** truncated: showing the first {shown} bytes of {total}.\n")
}

/// Read at most `cap` bytes of text. Returns the text, whether it was truncated, and the
/// file's real size. Invalid UTF-8 is replaced rather than refused — a text file with one
/// bad byte still deserves to render.
fn read_text_capped(path: &Path, cap: u64) -> io::Result<(String, bool, u64)> {
    let total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    // cap + 1 so a file that is exactly `cap` bytes is not reported as truncated.
    (&mut file).take(cap + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > cap;
    if truncated {
        bytes.truncate(cap as usize);
    }
    Ok((decode_text(&bytes, truncated), truncated, total))
}

/// Decode a text file's bytes. Invalid UTF-8 is replaced (a text file with one bad byte
/// still deserves to render) — except in the one case the cap itself creates: an
/// incomplete multi-byte character at the very end of a truncated read is dropped, so a
/// split character never shows up as a replacement character.
fn decode_text(bytes: &[u8], truncated: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(e) if truncated && e.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

// ---------------------------------------------------------------------------------------
// The PDF page probe
// ---------------------------------------------------------------------------------------

/// What the cheap PDF probe found.
struct PdfProbe {
    /// Whether the file starts with the `%PDF-` magic.
    has_header: bool,
    /// Page objects counted; `0` means "unknown", never "not a PDF".
    pages: u32,
    /// How many bytes the scan looked at.
    scanned: u64,
}

/// A **heuristic** page count for a PDF (§8 asks for `pages` probed, §7 records that the
/// host has no rasteriser).
///
/// This is a byte scan, not a parser: it counts occurrences of a page *object* dictionary —
/// `/Type`, optional whitespace, `/Page`, where the next byte is not `s` so the page *tree*
/// node `/Type /Pages` is not counted. It does not walk the page tree, and it does not
/// decompress object streams, so a PDF whose page objects live in an xref/compressed object
/// stream reports `0`; an incremental-update tail that repeats a page object can
/// over-count. `0` therefore means "unknown", never "not a PDF". The scan reads at most
/// [`MAX_PDF_SCAN_BYTES`] and streams in [`PDF_CHUNK`] chunks with an overlap so a needle
/// straddling a chunk boundary is still seen.
fn pdf_probe(path: &Path) -> io::Result<PdfProbe> {
    let mut file = File::open(path)?;

    let mut head = [0u8; 5];
    let mut read = 0usize;
    while read < head.len() {
        match file.read(&mut head[read..])? {
            0 => break,
            n => read += n,
        }
    }
    let has_header = &head[..read] == b"%PDF-";

    // Restart for the scan: the header read advanced the cursor.
    let mut file = File::open(path)?;
    let mut tail: Vec<u8> = Vec::new();
    // Global offset of `window[0]`, and the global end of the last page object counted:
    // the watermark is what makes the overlap safe (a needle seen at the end of one window
    // is decided — and counted — exactly once).
    let mut window_base: u64 = 0;
    let mut counted_through: u64 = 0;
    let mut buffer = vec![0u8; PDF_CHUNK];
    let mut pages: u32 = 0;
    let mut scanned: u64 = 0;
    let mut eof = false;

    while !eof {
        if scanned >= MAX_PDF_SCAN_BYTES {
            break;
        }
        let n = file.read(&mut buffer)?;
        if n == 0 {
            eof = true;
        }
        scanned += n as u64;

        let mut window = Vec::with_capacity(tail.len() + n);
        window.extend_from_slice(&tail);
        window.extend_from_slice(&buffer[..n]);
        if window.is_empty() {
            break;
        }
        let scan_len = window.len();
        let mut index = 0usize;
        while index < scan_len {
            match match_page_object(&window[index..]) {
                None => index += 1,
                Some(relative_end) => {
                    let end = index + relative_end;
                    if end == scan_len && !eof {
                        // The needle may continue as `/Pages` in the next chunk: leave it
                        // in the overlap and decide next round.
                        break;
                    }
                    let global_end = window_base + end as u64;
                    if global_end > counted_through {
                        // A match that ends before the watermark was counted in an
                        // earlier window; a deferred one was not, and is counted now.
                        pages = pages.saturating_add(1);
                        counted_through = global_end;
                    }
                    index = end;
                }
            }
        }

        let keep = NEEDLE_MAX.saturating_sub(1).min(scan_len);
        window_base += (scan_len - keep) as u64;
        tail = window[scan_len - keep..].to_vec();
    }

    Ok(PdfProbe {
        has_header,
        pages,
        scanned,
    })
}

/// The longest needle the matcher needs: `/Type` + up to 8 whitespace bytes + `/Page`.
const NEEDLE_MAX: usize = 5 + 8 + 5;

/// One byte of PDF whitespace (plus NUL, which appears in some producers' padding).
fn is_pdf_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'\0' | 0x0c)
}

/// Match `/Type<space>* /Page` at the start of `window`; returns the end offset of the
/// match. Refuses `/Pages` (the page tree node) by inspecting the byte after `/Page`.
fn match_page_object(window: &[u8]) -> Option<usize> {
    if !window.starts_with(b"/Type") {
        return None;
    }
    let mut i = 5;
    while i < window.len() && i < 5 + 8 && is_pdf_space(window[i]) {
        i += 1;
    }
    if !window[i..].starts_with(b"/Page") {
        return None;
    }
    let end = i + 5;
    // `/Page` with no following byte yet (end of a non-final window) is handled by the
    // caller's overlap logic; here it counts only when the next byte is not `s`.
    if end < window.len() && window[end] == b's' {
        return None;
    }
    Some(end)
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway workspace root. Built on `std::env::temp_dir` so the test suite needs no
    /// extra dependency; removed on drop.
    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> TempRoot {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!("qfr-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp root");
            TempRoot { path }
        }

        fn join(&self, rel: &str) -> PathBuf {
            self.path.join(rel)
        }

        /// Write a file (creating parents) and return its path.
        fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&path, bytes).expect("write fixture");
            path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn root_str(root: &TempRoot) -> String {
        root.path.to_string_lossy().into_owned()
    }

    /// A §3 request for one renderer with the given `file@1` attrs.
    fn request(renderer: &str, root: &TempRoot, attrs: Value) -> String {
        json!({
            "abi": 1,
            "renderer": renderer,
            "note_id": "n-test",
            "workspace_root": root_str(root),
            "block": { "id": "b-test", "type_id": FILE_TYPE_ID, "text": "", "attrs": attrs },
        })
        .to_string()
    }

    fn response(json: &str) -> Value {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("response is not JSON ({e}): {json}"))
    }

    fn plan_of(json: &str) -> Value {
        let value = response(json);
        match value.get("plan") {
            Some(plan) => plan.clone(),
            None => panic!("expected a plan, got {json}"),
        }
    }

    fn error_of(json: &str) -> String {
        let value = response(json);
        match value.get("error").and_then(Value::as_str) {
            Some(message) => message.to_string(),
            None => panic!("expected an error, got {json}"),
        }
    }

    fn kind_of(json: &str) -> String {
        plan_of(json)["kind"].as_str().unwrap().to_string()
    }

    // --- identity, descriptor, manifest, plugin.json -----------------------------------

    #[test]
    fn the_abi_entry_point_returns_one() {
        assert_eq!(PLUGIN_ABI_VERSION, 1);
        assert_eq!(quantum_plugin_v1_abi(), 1);
    }

    #[test]
    fn describe_is_a_nul_terminated_descriptor_object() {
        assert_eq!(DESCRIPTOR_CSTR.as_bytes().last(), Some(&0));
        let value: Value = serde_json::from_str(descriptor_json()).expect("describe() is JSON");
        assert_eq!(value["abi"], json!(1));
        assert_eq!(value["id"], json!(PLUGIN_ID));
        assert_eq!(value["version"], json!(PLUGIN_VERSION));
        assert!(
            value["renderers"].is_array(),
            "the descriptor carries the renderer list"
        );
    }

    #[test]
    fn describe_lists_exactly_the_three_documented_renderers() {
        let value: Value = serde_json::from_str(descriptor_json()).unwrap();
        let ids: Vec<String> = value["renderers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ids,
            vec![
                RENDERER_MEDIA.to_string(),
                RENDERER_MARKDOWN.to_string(),
                RENDERER_DOCUMENT.to_string()
            ]
        );
        // §2: ids unique, types/mimes non-empty, priority an i32.
        let mut seen = std::collections::BTreeSet::new();
        for renderer in value["renderers"].as_array().unwrap() {
            assert!(seen.insert(renderer["id"].as_str().unwrap()));
            assert!(!renderer["types"].as_array().unwrap().is_empty());
            assert!(!renderer["mimes"].as_array().unwrap().is_empty());
            assert!(renderer["priority"].as_i64().is_some());
            assert_eq!(renderer["types"][0], json!(FILE_TYPE_ID));
        }
        assert_eq!(
            value["renderers"][2]["mimes"],
            json!(["application/pdf", "*/*"])
        );
    }

    #[test]
    fn describe_matches_plugin_json_renderers() {
        // The install descriptor and the runtime descriptor must not drift (§3): the host
        // refuses an activation whose describe() disagrees with plugin.json.
        let describe: Value = serde_json::from_str(descriptor_json()).unwrap();
        let plugin: Value =
            serde_json::from_str(include_str!("../plugin.json")).expect("plugin.json is JSON");
        assert_eq!(describe["abi"], plugin["abi"]);
        assert_eq!(describe["id"], plugin["id"]);
        assert_eq!(describe["version"], plugin["version"]);
        assert_eq!(describe["renderers"], plugin["renderers"]);
    }

    #[test]
    fn plugin_json_declares_a_contained_entry_and_a_64_hex_digest() {
        let plugin: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        assert_eq!(plugin["abi"], json!(PLUGIN_ABI_VERSION));
        assert_eq!(plugin["id"], json!(PLUGIN_ID));
        assert_eq!(plugin["version"], json!(PLUGIN_VERSION));
        let entry = plugin["entry"].as_str().expect("entry is a string");
        assert!(
            entry.starts_with("lib/"),
            "entry lives in lib/ (§2): {entry}"
        );
        assert!(
            !entry.contains("..") && !entry.starts_with('/'),
            "entry must stay inside the bundle root (§2): {entry}"
        );
        let sha = plugin["entry_sha256"].as_str().expect("entry_sha256");
        assert_eq!(
            sha.len(),
            64,
            "entry_sha256 is 64 hex chars (§2); run tools/bundle.sh"
        );
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit()),
            "entry_sha256: {sha}"
        );
    }

    #[test]
    fn manifest_is_the_d21_shape_with_an_empty_register_list() {
        let manifest = include_str!("../manifest.toml");
        for expected in [
            "id = \"file-renderer\"",
            "version = \"0.1.0\"",
            "api-range = \">=0.1, <0.2\"",
            "capabilities = []",
            "provides = []",
            "registers = []",
        ] {
            assert!(
                manifest.contains(expected),
                "manifest.toml must contain `{expected}`:\n{manifest}"
            );
        }
        assert!(
            !manifest.contains("[[registers]]"),
            "a renderer-only plugin registers no types (§2)"
        );
    }

    // --- plan kinds -------------------------------------------------------------------

    #[test]
    fn an_image_block_yields_an_image_plan() {
        let root = TempRoot::new("image");
        root.write("pics/cat.png", b"\x89PNG\r\n\x1a\n not really a png");
        let json = render_json(&request(
            RENDERER_MEDIA,
            &root,
            json!({ "path": "pics/cat.png", "name": "cat.png", "mime": "image/png" }),
        ));
        let plan = plan_of(&json);
        assert_eq!(plan["kind"], "image");
        assert_eq!(plan["alt"], "cat.png");
        assert_eq!(
            plan["source"].as_str().unwrap(),
            root_str(&root) + "/pics/cat.png",
            "source is the absolute resolved path (§4)"
        );
    }

    #[test]
    fn a_video_block_yields_a_video_card_plan() {
        let root = TempRoot::new("video");
        root.write("clips/demo.mp4", b"\x00\x00\x00\x18ftypmp42");
        let json = render_json(&request(
            RENDERER_MEDIA,
            &root,
            json!({ "path": "clips/demo.mp4", "name": "demo.mp4", "mime": "video/mp4" }),
        ));
        let plan = plan_of(&json);
        assert_eq!(plan["kind"], "video");
        assert_eq!(plan["caption"], "demo.mp4");
        assert!(
            plan["duration_ms"].is_null(),
            "no parser, so no claimed duration_ms"
        );
    }

    #[test]
    fn a_markdown_block_yields_a_markdown_plan_with_the_file_text() {
        let root = TempRoot::new("md");
        root.write("docs/spec.md", b"# Spec\n\nBody text.\n");
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "docs/spec.md", "name": "spec.md", "mime": "text/markdown" }),
        ));
        let plan = plan_of(&json);
        assert_eq!(plan["kind"], "markdown");
        assert_eq!(plan["title"], "spec.md");
        assert_eq!(plan["text"], "# Spec\n\nBody text.\n");
    }

    #[test]
    fn plain_text_renders_through_the_markdown_renderer() {
        let root = TempRoot::new("txt");
        root.write("notes.txt", b"plain\n");
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "notes.txt", "name": "notes.txt", "mime": "text/plain" }),
        ));
        assert_eq!(kind_of(&json), "markdown");
        assert_eq!(plan_of(&json)["text"], "plain\n");
    }

    #[test]
    fn a_pdf_block_yields_a_document_card_with_a_probed_page_count() {
        let root = TempRoot::new("pdf");
        // Three page objects plus the page TREE node, which must not be counted.
        root.write(
            "docs/paper.pdf",
            b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
              2 0 obj\n<< /Type /Pages /Kids [3 0 R 5 0 R 7 0 R] /Count 3 >>\nendobj\n\
              3 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n\
              5 0 obj\n<< /Type /Page >>\nendobj\n\
              7 0 obj\n<< /Type/Page>>\nendobj\n%%EOF\n",
        );
        let json = render_json(&request(
            RENDERER_DOCUMENT,
            &root,
            json!({ "path": "docs/paper.pdf", "name": "paper.pdf", "mime": "application/pdf" }),
        ));
        let plan = plan_of(&json);
        assert_eq!(plan["kind"], "document");
        assert_eq!(plan["mime"], "application/pdf");
        assert_eq!(plan["title"], "paper.pdf");
        assert_eq!(
            plan["pages"], 3,
            "the /Pages tree node must not be counted: {plan}"
        );
        assert_eq!(
            plan["source"].as_str().unwrap(),
            root_str(&root) + "/docs/paper.pdf"
        );
    }

    #[test]
    fn an_unparseable_pdf_still_renders_a_card_with_a_note() {
        let root = TempRoot::new("pdf-bad");
        root.write("docs/scan.pdf", b"not a pdf at all");
        let json = render_json(&request(
            RENDERER_DOCUMENT,
            &root,
            json!({ "path": "docs/scan.pdf", "name": "scan.pdf", "mime": "application/pdf" }),
        ));
        let plan = plan_of(&json);
        assert_eq!(plan["kind"], "document");
        assert!(plan["pages"].is_null());
        assert!(plan["note"].as_str().unwrap().contains("%PDF-"), "{plan}");
    }

    #[test]
    fn the_pdf_probe_survives_a_needle_on_a_chunk_boundary() {
        let root = TempRoot::new("pdf-boundary");
        // Straddle the 64 KiB read boundary: the first needle's `/Type ` ends chunk one and
        // its `/Page` starts chunk two, so the matcher must decide across the overlap.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"%PDF-1.7\n");
        bytes.extend(std::iter::repeat_n(b'x', PDF_CHUNK - 15));
        bytes.extend_from_slice(b"/Type /Page\n");
        bytes.extend(std::iter::repeat_n(b'y', 64));
        bytes.extend_from_slice(b"/Type /Page\n");
        root.write("straddle.pdf", &bytes);

        let probe = pdf_probe(&root.join("straddle.pdf")).expect("probe");
        assert!(probe.has_header);
        assert_eq!(
            probe.pages, 2,
            "the boundary needle must be counted exactly once"
        );
        assert_eq!(
            probe.scanned,
            bytes.len() as u64,
            "the whole file was scanned"
        );
    }

    #[test]
    fn the_pdf_probe_excludes_the_pages_tree_node() {
        let root = TempRoot::new("pdf-tree");
        // `/Page` immediately followed by `s` is the page tree; `Pages` split across the
        // window boundary must not be counted either.
        root.write(
            "tree.pdf",
            b"%PDF-1.4\n<< /Type /Pages /Count 0 >>\n<< /Type /Page >>\n",
        );
        let probe = pdf_probe(&root.join("tree.pdf")).expect("probe");
        assert_eq!(probe.pages, 1, "{probe_pages}", probe_pages = probe.pages);
    }

    #[test]
    fn an_unknown_mime_falls_through_to_the_document_card() {
        let root = TempRoot::new("doc-unknown");
        root.write("data/archive.zip", b"PK\x03\x04");
        let json = render_json(&request(
            RENDERER_DOCUMENT,
            &root,
            json!({ "path": "data/archive.zip", "name": "archive.zip", "mime": "application/zip" }),
        ));
        let plan = plan_of(&json);
        assert_eq!(plan["kind"], "document");
        assert_eq!(plan["mime"], "application/zip");
    }

    #[test]
    fn an_empty_mime_renders_as_an_octet_stream_document() {
        let root = TempRoot::new("doc-nomime");
        root.write("data/blob.bin", b"\x00\x01\x02");
        let json = render_json(&request(
            RENDERER_DOCUMENT,
            &root,
            json!({ "path": "data/blob.bin", "name": "blob.bin" }),
        ));
        let plan = plan_of(&json);
        assert_eq!(plan["kind"], "document");
        assert_eq!(plan["mime"], DEFAULT_MIME);
    }

    // --- markdown cap -----------------------------------------------------------------

    #[test]
    fn markdown_is_size_capped_with_a_truncation_note() {
        let root = TempRoot::new("cap");
        let tail = b"TAIL-MARKER-THAT-MUST-NOT-APPEAR";
        let mut content = vec![b'a'; MAX_TEXT_BYTES as usize];
        content.extend_from_slice(tail);
        root.write("big.md", &content);

        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "big.md", "name": "big.md", "mime": "text/markdown" }),
        ));
        let plan = plan_of(&json);
        let text = plan["text"].as_str().unwrap();
        // Never print the whole plan into a failure message.
        let brief: String = text.chars().take(80).collect();
        assert!(
            !text.contains("TAIL-MARKER-THAT-MUST-NOT-APPEAR"),
            "the cap must cut the tail"
        );
        assert!(text.contains("truncated: showing the first"), "{brief}");
        assert!(
            text.contains(&(MAX_TEXT_BYTES + tail.len() as u64).to_string()),
            "the note names the real size: {brief}"
        );
        assert!(text.len() as u64 <= MAX_TEXT_BYTES + 256);
    }

    #[test]
    fn markdown_exactly_at_the_cap_is_not_reported_as_truncated() {
        let root = TempRoot::new("cap-exact");
        root.write("exact.md", &vec![b'z'; MAX_TEXT_BYTES as usize]);
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "exact.md", "name": "exact.md", "mime": "text/markdown" }),
        ));
        let text = plan_of(&json)["text"].as_str().unwrap().to_string();
        assert_eq!(text.len() as u64, MAX_TEXT_BYTES);
        assert!(!text.contains("truncated"));
    }

    #[test]
    fn a_cap_that_splits_a_character_keeps_the_text_valid_utf8() {
        let root = TempRoot::new("cap-utf8");
        // `é` is two bytes, so a cap landing inside it must be pulled back to a character
        // boundary: no replacement character and no half character in the plan.
        let mut content = vec![b'x'; (MAX_TEXT_BYTES as usize) - 1];
        content.extend_from_slice("éé".as_bytes());
        root.write("utf8.md", &content);
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "utf8.md", "name": "utf8.md", "mime": "text/markdown" }),
        ));
        let plan = plan_of(&json);
        let text = plan["text"].as_str().unwrap();
        assert!(
            text.starts_with("xxxx"),
            "the text before the split is kept"
        );
        assert!(
            text.contains("truncated: showing the first"),
            "the note is still appended"
        );
        assert!(
            !text.contains('\u{FFFD}'),
            "a character split by the cap is dropped, not replaced"
        );
    }

    #[test]
    fn invalid_utf8_in_a_text_file_is_replaced_not_refused() {
        let root = TempRoot::new("bad-utf8");
        root.write("bad.txt", b"ok\xffbad\n");
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "bad.txt", "name": "bad.txt", "mime": "text/plain" }),
        ));
        let text = plan_of(&json)["text"].as_str().unwrap().to_string();
        assert!(text.contains("ok") && text.contains("bad"));
        assert!(text.contains('\u{FFFD}'));
    }

    // --- refusals ---------------------------------------------------------------------

    #[test]
    fn a_missing_file_is_refused_with_an_error_plan() {
        let root = TempRoot::new("missing");
        let json = render_json(&request(
            RENDERER_MEDIA,
            &root,
            json!({ "path": "nope.png", "name": "nope.png", "mime": "image/png" }),
        ));
        let message = error_of(&json);
        assert!(message.contains("nope.png"), "{message}");
        assert!(
            json.contains(root_str(&root).as_str()),
            "the resolved absolute path is named: {json}"
        );
    }

    #[test]
    fn a_missing_workspace_file_is_refused_for_every_renderer() {
        let root = TempRoot::new("missing-all");
        for (renderer, mime) in [
            (RENDERER_MEDIA, "video/mp4"),
            (RENDERER_MARKDOWN, "text/markdown"),
            (RENDERER_DOCUMENT, "application/pdf"),
        ] {
            let json = render_json(&request(
                renderer,
                &root,
                json!({ "path": "gone.bin", "mime": mime }),
            ));
            assert!(response(&json).get("error").is_some(), "{renderer}: {json}");
        }
    }

    #[test]
    fn a_path_that_escapes_the_root_is_refused_without_reading_it() {
        let root = TempRoot::new("escape");
        // A secret one level above the root; the block's path climbs to it.
        let outside = root
            .path
            .parent()
            .unwrap()
            .join(format!("qfr-secret-{}-escape.txt", std::process::id()));
        std::fs::write(&outside, b"TOP-SECRET-CONTENTS").unwrap();

        let escaping = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": &escaping, "mime": "text/plain" }),
        ));
        let message = error_of(&json);
        assert!(message.contains("escapes"), "{message}");
        assert!(
            !json.contains("TOP-SECRET-CONTENTS"),
            "the escaped file must never be read: {json}"
        );

        // The same file named by its real relative name works, proving the guard is about
        // containment, not about refusing everything.
        std::fs::write(root.join("ok.txt"), b"inside").unwrap();
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "ok.txt", "mime": "text/plain" }),
        ));
        assert_eq!(plan_of(&json)["text"], "inside");
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn a_path_that_climbs_above_the_root_mid_path_is_refused() {
        let root = TempRoot::new("escape-mid");
        root.write("docs/spec.md", b"# hi\n");
        for escaping in ["docs/../../etc/passwd", "a/b/../../../x", ".."] {
            let json = render_json(&request(
                RENDERER_MARKDOWN,
                &root,
                json!({ "path": escaping, "mime": "text/markdown" }),
            ));
            let message = error_of(&json);
            assert!(
                message.contains("escapes") || message.contains("does not name a file"),
                "`{escaping}`: {message}"
            );
        }
    }

    #[test]
    fn a_parent_dir_that_stays_inside_the_root_is_allowed() {
        let root = TempRoot::new("inside-parent");
        root.write("docs/spec.md", b"# spec\n");
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "docs/../docs/spec.md", "mime": "text/markdown" }),
        ));
        assert_eq!(plan_of(&json)["kind"], "markdown");
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let root = TempRoot::new("absolute");
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "/etc/hosts", "mime": "text/plain" }),
        ));
        let message = error_of(&json);
        assert!(
            message.contains("relative to the workspace root"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_root_is_refused_by_the_canonicalising_guard() {
        let root = TempRoot::new("symlink");
        let outside = root
            .path
            .parent()
            .unwrap()
            .join(format!("qfr-symlink-target-{}.txt", std::process::id()));
        std::fs::write(&outside, b"LINKED-SECRET").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link.txt")).unwrap();

        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "link.txt", "mime": "text/plain" }),
        ));
        let message = error_of(&json);
        assert!(message.contains("escapes"), "{message}");
        assert!(!json.contains("LINKED-SECRET"), "{json}");
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn a_directory_is_refused() {
        let root = TempRoot::new("dir");
        root.write("docs/spec.md", b"# hi\n");
        let json = render_json(&request(
            RENDERER_DOCUMENT,
            &root,
            json!({ "path": "docs", "mime": "application/pdf" }),
        ));
        let message = error_of(&json);
        assert!(message.contains("directory"), "{message}");
    }

    #[test]
    fn media_refuses_a_mime_it_does_not_handle() {
        let root = TempRoot::new("media-mime");
        root.write("data.zip", b"PK\x03\x04");
        let json = render_json(&request(
            RENDERER_MEDIA,
            &root,
            json!({ "path": "data.zip", "mime": "application/zip" }),
        ));
        let message = error_of(&json);
        assert!(
            message.contains("image/*") && message.contains("application/zip"),
            "{message}"
        );
        assert!(response(&json).get("plan").is_none());
    }

    #[test]
    fn media_refuses_an_empty_mime() {
        let root = TempRoot::new("media-empty-mime");
        root.write("pic.png", b"\x89PNG");
        let json = render_json(&request(
            RENDERER_MEDIA,
            &root,
            json!({ "path": "pic.png" }),
        ));
        let message = error_of(&json);
        assert!(message.contains("(none)"), "{message}");
    }

    #[test]
    fn markdown_refuses_a_mime_it_does_not_handle() {
        let root = TempRoot::new("md-mime");
        root.write("paper.pdf", b"%PDF-1.4\n");
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "paper.pdf", "mime": "application/pdf" }),
        ));
        let message = error_of(&json);
        assert!(message.contains("text/markdown"), "{message}");
    }

    #[test]
    fn an_unknown_renderer_is_refused() {
        let root = TempRoot::new("unknown-renderer");
        let json = render_json(&request("file-renderer.nope", &root, json!({})));
        let message = error_of(&json);
        assert!(message.contains("unknown renderer") && message.contains("file-renderer.nope"));
    }

    #[test]
    fn a_block_whose_type_is_not_file_1_is_refused() {
        let root = TempRoot::new("wrong-type");
        let json = render_json(
            &json!({
                "abi": 1,
                "renderer": RENDERER_MEDIA,
                "workspace_root": root_str(&root),
                "block": { "id": "b", "type_id": "task@2", "text": "", "attrs": {} },
            })
            .to_string(),
        );
        let message = error_of(&json);
        assert!(
            message.contains("file@1") && message.contains("task@2"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_path_attr_is_refused() {
        let root = TempRoot::new("no-path");
        let json = render_json(&request(
            RENDERER_MEDIA,
            &root,
            json!({ "mime": "image/png" }),
        ));
        let message = error_of(&json);
        assert!(message.contains("path"), "{message}");
    }

    #[test]
    fn a_missing_workspace_root_is_refused() {
        let json = render_json(
            &json!({
                "abi": 1,
                "renderer": RENDERER_MEDIA,
                "block": { "id": "b", "type_id": FILE_TYPE_ID, "text": "", "attrs": { "path": "a.png", "mime": "image/png" } },
            })
            .to_string(),
        );
        assert!(error_of(&json).contains("workspace_root"));
    }

    #[test]
    fn a_missing_block_is_refused() {
        let root = TempRoot::new("no-block");
        let json = render_json(
            &json!({ "abi": 1, "renderer": RENDERER_MEDIA, "workspace_root": root_str(&root) })
                .to_string(),
        );
        assert!(error_of(&json).contains("block"));
    }

    #[test]
    fn a_renderer_field_that_is_empty_is_refused() {
        let root = TempRoot::new("empty-renderer");
        let json = render_json(&request("  ", &root, json!({ "path": "a.png" })));
        assert!(error_of(&json).contains("renderer"));
    }

    #[test]
    fn an_unsupported_request_abi_is_refused() {
        let root = TempRoot::new("bad-abi");
        let json = render_json(
            &json!({
                "abi": 2,
                "renderer": RENDERER_MEDIA,
                "workspace_root": root_str(&root),
                "block": { "id": "b", "type_id": FILE_TYPE_ID, "text": "", "attrs": { "path": "a.png", "mime": "image/png" } },
            })
            .to_string(),
        );
        let message = error_of(&json);
        assert!(
            message.contains("abi 2") && message.contains("speaks 1"),
            "{message}"
        );
    }

    #[test]
    fn malformed_request_json_is_refused() {
        let message = error_of(&render_json("{ not json"));
        assert!(message.contains("malformed request JSON"), "{message}");
    }

    #[test]
    fn an_absent_abi_field_is_tolerated() {
        let root = TempRoot::new("no-abi");
        root.write("pic.png", b"\x89PNG");
        let json = render_json(
            &json!({
                "renderer": RENDERER_MEDIA,
                "workspace_root": root_str(&root),
                "block": { "id": "b", "type_id": FILE_TYPE_ID, "text": "", "attrs": { "path": "pic.png", "mime": "image/png" } },
            })
            .to_string(),
        );
        assert_eq!(kind_of(&json), "image");
    }

    #[test]
    fn numeric_and_boolean_attrs_are_coerced_not_refused() {
        let root = TempRoot::new("attr-coercion");
        root.write("pic.png", b"\x89PNG");
        let json = render_json(&request(
            RENDERER_MEDIA,
            &root,
            json!({ "path": "pic.png", "name": 42, "mime": "image/png", "size": 1234, "hidden": true }),
        ));
        assert_eq!(plan_of(&json)["alt"], "42");
    }

    #[test]
    fn a_name_attr_that_is_missing_or_blank_falls_back_to_file() {
        let root = TempRoot::new("no-name");
        root.write("pic.png", b"\x89PNG");
        for attrs in [
            json!({ "path": "pic.png", "mime": "image/png" }),
            json!({ "path": "pic.png", "name": "   ", "mime": "image/png" }),
        ] {
            let json = render_json(&request(RENDERER_MEDIA, &root, attrs));
            assert_eq!(plan_of(&json)["alt"], "file");
        }
    }

    // --- panic containment and the raw ABI --------------------------------------------

    #[test]
    fn a_panic_becomes_an_error_response() {
        let response = guard(|| panic!("boom"));
        let message = error_of(&response);
        assert!(
            message.contains("plugin panicked") && message.contains("boom"),
            "{message}"
        );
    }

    #[test]
    fn a_string_panic_payload_is_reported() {
        let response = guard(|| panic!("{}", String::from("owned message")));
        assert!(error_of(&response).contains("owned message"));
    }

    #[test]
    fn guard_passes_a_normal_response_through_unchanged() {
        let response = guard(|| "{\"plan\":{\"kind\":\"fallback\"}}".to_string());
        assert_eq!(response, "{\"plan\":{\"kind\":\"fallback\"}}");
    }

    #[test]
    fn the_c_abi_round_trips_through_raw_pointers() {
        let root = TempRoot::new("abi");
        root.write("pics/cat.png", b"\x89PNG\r\n\x1a\n");

        assert_eq!(quantum_plugin_v1_abi(), 1);

        // describe(): static and NUL-terminated, and the same bytes the host receives.
        let described = unsafe { CStr::from_ptr(quantum_plugin_v1_describe()) };
        assert_eq!(described.to_bytes(), descriptor_json().as_bytes());

        // render(): owned response, freed by free().
        let request_c = CString::new(request(
            RENDERER_MEDIA,
            &root,
            json!({ "path": "pics/cat.png", "name": "cat.png", "mime": "image/png" }),
        ))
        .unwrap();
        let out = unsafe { quantum_plugin_v1_render(request_c.as_ptr()) };
        assert!(!out.is_null());
        let rendered = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { quantum_plugin_v1_free(out) };
        assert_eq!(kind_of(&rendered), "image");

        // A null request is an error response, never a crash.
        let out = unsafe { quantum_plugin_v1_render(std::ptr::null()) };
        let message = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { quantum_plugin_v1_free(out) };
        assert!(error_of(&message).contains("null"), "{message}");

        // free(null) is a no-op.
        unsafe { quantum_plugin_v1_free(std::ptr::null_mut()) };
    }

    #[test]
    fn a_panic_inside_the_render_entry_point_is_contained() {
        // The test-only renderer panics deep inside `render_plan`; the C entry point must
        // still return an owned error response rather than unwinding into the host (§3).
        let root = TempRoot::new("abi-panic");
        let request_c = CString::new(request(TEST_PANIC_RENDERER, &root, json!({}))).unwrap();
        let out = unsafe { quantum_plugin_v1_render(request_c.as_ptr()) };
        assert!(!out.is_null());
        let response = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { quantum_plugin_v1_free(out) };
        let message = error_of(&response);
        assert!(
            message.contains("plugin panicked") && message.contains("test panic probe"),
            "{message}"
        );
    }

    #[test]
    fn a_non_utf8_request_is_refused() {
        // No interior NUL, so `CString::new` accepts it: a valid C string that is not UTF-8.
        let bad = CString::new(vec![0xff, 0xfe]).unwrap();
        let out = unsafe { quantum_plugin_v1_render(bad.as_ptr()) };
        let response = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
        unsafe { quantum_plugin_v1_free(out) };
        assert!(error_of(&response).contains("UTF-8"));
    }

    #[test]
    fn a_response_never_contains_an_interior_nul() {
        let root = TempRoot::new("nul");
        root.write("odd.txt", b"has no nul");
        let json = render_json(&request(
            RENDERER_MARKDOWN,
            &root,
            json!({ "path": "odd.txt", "mime": "text/plain" }),
        ));
        let c = CString::new(json.clone()).expect("no interior NUL");
        assert_eq!(c.to_bytes(), json.as_bytes());
    }

    // --- path resolution, directly ----------------------------------------------------

    #[test]
    fn resolve_below_root_normalises_but_never_escapes() {
        let root = Path::new("/tmp/ws");
        assert_eq!(
            resolve_below_root(root, "docs/spec.md").unwrap(),
            PathBuf::from("/tmp/ws/docs/spec.md")
        );
        assert_eq!(
            resolve_below_root(root, "./docs/../docs/spec.md").unwrap(),
            PathBuf::from("/tmp/ws/docs/spec.md")
        );
        for bad in [
            "../outside.md",
            "docs/../../outside.md",
            "/etc/passwd",
            "..",
            "",
            "   ",
        ] {
            assert!(
                resolve_below_root(root, bad).is_err(),
                "`{bad}` must be refused"
            );
        }
    }

    #[test]
    fn the_descriptor_is_static_memory() {
        let first = quantum_plugin_v1_describe();
        let second = quantum_plugin_v1_describe();
        assert_eq!(first, second, "describe() must hand out one static pointer");
    }
}
