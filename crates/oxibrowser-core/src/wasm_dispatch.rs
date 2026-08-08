//! JSON-in/JSON-out verb surface for the wasm32-wasip1 build, callable from
//! gm's agentplug runtime. Mirrors the real loadable-plugin contract used by
//! `agentplug-treesitter`/`agentplug-bert`/`agentplug-libsql` (verified by
//! reading `../gm/agentplug-treesitter/src/abi.rs` directly) — NOT
//! plugkit-core's `dispatch_verb`, which is the orchestrator's own entry
//! point, not what a *loaded* plugin exports. A loadable plugin exports
//! three functions: `plugkit_alloc`/`plugkit_free` (so the host can write
//! verb/body bytes into this instance's linear memory before calling in) and
//! `plugin_call(verb_ptr, verb_len, body_ptr, body_len) -> u64` (packed
//! ptr+len result, allocated via `plugkit_alloc` so the host frees it the
//! same way).
//!
//! Verbs: `navigate`, `evaluate`, `dom-query`, `extract-markdown`,
//! `capabilities`. Each keeps a single `Session` alive for the lifetime of
//! the wasm instance (`SESSION` thread-local) — a real browser tab, not a
//! stateless RPC.
#![cfg(not(feature = "native"))]

use crate::browser::BrowserId;
use crate::config::BrowserConfig;
use crate::network::HttpClient;
use crate::network::cookie::CookieJar;
use parking_lot::RwLock;
use crate::session::Session;
use serde_json::{Value, json};
use std::alloc::{Layout, alloc, dealloc};
use std::cell::RefCell;
use std::mem;

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

#[unsafe(no_mangle)]
pub extern "C" fn plugkit_alloc(len: u32) -> u32 {
    if len == 0 {
        return 0;
    }
    let layout = Layout::from_size_align(len as usize, mem::align_of::<u8>()).unwrap();
    unsafe { alloc(layout) as u32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn plugkit_free(ptr: u32, len: u32) {
    if ptr == 0 || len == 0 {
        return;
    }
    let layout = Layout::from_size_align(len as usize, mem::align_of::<u8>()).unwrap();
    unsafe { dealloc(ptr as *mut u8, layout) };
}

fn read_str(ptr: u32, len: u32) -> String {
    if ptr == 0 || len == 0 {
        return String::new();
    }
    unsafe {
        let slice = std::slice::from_raw_parts(ptr as *const u8, len as usize);
        String::from_utf8_lossy(slice).into_owned()
    }
}

fn return_bytes(bytes: Vec<u8>) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let len = bytes.len();
    let ptr = plugkit_alloc(len as u32);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, len);
    }
    (ptr as u64 & 0xffff_ffff) | ((len as u64) << 32)
}

fn ok(verb: &str, data: Value) -> u64 {
    return_bytes(json!({ "ok": true, "verb": verb, "data": data }).to_string().into_bytes())
}

fn err(verb: &str, message: impl Into<String>) -> u64 {
    return_bytes(
        json!({ "ok": false, "verb": verb, "error": message.into() })
            .to_string()
            .into_bytes(),
    )
}

/// The host's single call surface: `verb` selects the operation, `body` is
/// its JSON argument, both written into this instance's memory via
/// `plugkit_alloc` before the call. Returns a packed ptr+len result the host
/// reads then frees with `plugkit_free`.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_call(verb_ptr: u32, verb_len: u32, body_ptr: u32, body_len: u32) -> u64 {
    let verb = read_str(verb_ptr, verb_len);
    let body_s = read_str(body_ptr, body_len);
    let body: Value = if body_s.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body_s).unwrap_or(Value::Null)
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch_verb_inner(&verb, &body)
    }));
    match result {
        Ok(packed) => packed,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic during dispatch".to_string());
            err(&verb, format!("panicked: {msg}"))
        }
    }
}

fn dispatch_verb_inner(verb: &str, body: &Value) -> u64 {
    // Every verb runs the session's tiny single-threaded async engine
    // to completion synchronously — matches the wasm build's overall
    // single-threaded execution model (see js/runtime.rs's InlineJsEngine).
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(e) => return err(verb, format!("failed to build async runtime: {e}")),
    };
    rt.block_on(async { dispatch_verb_async(verb, body).await })
}

async fn dispatch_verb_async(verb: &str, body: &Value) -> u64 {
    match verb {
        "navigate" => verb_navigate(body).await,
        "evaluate" => verb_evaluate(body).await,
        "dom-query" => verb_dom_query(body).await,
        "extract-markdown" => verb_extract_markdown().await,
        // Answers "what can you do" without side effects — same convention
        // as agentplug-treesitter's capabilities verb — so a caller can
        // probe before dispatching instead of discovering a missing verb as
        // an indistinguishable ok:false at the call site.
        "capabilities" => ok(
            "capabilities",
            json!({
                "plugin": "oxibrowser",
                "verbs": ["navigate", "evaluate", "dom-query", "extract-markdown", "capabilities"],
                "payload_field": {
                    "navigate": "navigated",
                    "evaluate": "value",
                    "dom-query": "nodes",
                    "extract-markdown": "markdown",
                },
            }),
        ),
        other => err(other, format!("unknown verb: {other}")),
    }
}

async fn with_session<F, T>(f: F) -> Result<T, String>
where
    F: for<'a> FnOnce(
        &'a mut Session,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::error::Result<T>> + 'a>>,
{
    let mut cell = SESSION.take();
    if cell.is_none() {
        // Build a Session directly rather than via Browser::new_session():
        // Browser keeps its own Arc clone in `self.sessions` for multi-tab
        // tracking, which made an earlier version of this function fail
        // Arc::try_unwrap unconditionally (confirmed live via a real
        // plugin_call dispatch through gm's agentplug daemon: {"error":
        // "session Arc has other owners"}). The wasm verb surface only ever
        // runs one session per instance and has no use for Browser's
        // multi-tab bookkeeping, so skip it entirely.
        let config = BrowserConfig::headless();
        let cookie_jar = Arc::new(RwLock::new(CookieJar::new()));
        let http_client =
            Arc::new(HttpClient::new(&config, cookie_jar.clone()).map_err(|e| e.to_string())?);
        let session = Session::new(BrowserId::next(), config, http_client, cookie_jar)
            .await
            .map_err(|e| e.to_string())?;
        cell = Some(session);
    }
    let mut session = cell.expect("cell populated above");
    let result = f(&mut session).await.map_err(|e| e.to_string());
    SESSION.set(Some(session));
    result
}

use std::sync::Arc;

async fn verb_navigate(body: &Value) -> u64 {
    let Some(url) = body.get("url").and_then(|v| v.as_str()) else {
        return err("navigate", "missing required field: url");
    };
    let url = url.to_string();
    match with_session(|session| Box::pin(async move { session.navigate(&url).await })).await
    {
        Ok(()) => ok("navigate", json!({ "navigated": true })),
        Err(e) => err("navigate", e),
    }
}

async fn verb_evaluate(body: &Value) -> u64 {
    let Some(expression) = body.get("expression").and_then(|v| v.as_str()) else {
        return err("evaluate", "missing required field: expression");
    };
    let expression = expression.to_string();
    match with_session(|session| Box::pin(async move { session.evaluate_js(&expression).await }))
        .await
    {
        Ok(result) => ok(
            "evaluate",
            json!({ "value": result.value, "exception": result.exception }),
        ),
        Err(e) => err("evaluate", e),
    }
}

async fn verb_dom_query(body: &Value) -> u64 {
    let Some(selector) = body.get("selector").and_then(|v| v.as_str()) else {
        return err("dom-query", "missing required field: selector");
    };
    let selector = selector.to_string();
    match with_session(|session| Box::pin(async move { session.dom_snapshot().await })).await
    {
        Ok(Some(snapshot)) => {
            let matched = snapshot.query_selector_all(&selector);
            let nodes: Vec<Value> = matched
                .into_iter()
                .filter_map(|id| snapshot.nodes.get(&id))
                .map(|n| json!({ "tag": n.tag, "text": n.text_content }))
                .collect();
            ok("dom-query", json!({ "nodes": nodes }))
        }
        Ok(None) => ok("dom-query", json!({ "nodes": [] })),
        Err(e) => err("dom-query", e),
    }
}

async fn verb_extract_markdown() -> u64 {
    match with_session(|session| {
        Box::pin(async move { Ok(session.page().map(|p| p.to_markdown()).unwrap_or_default()) })
    })
    .await
    {
        Ok(markdown) => ok("extract-markdown", json!({ "markdown": markdown })),
        Err(e) => err("extract-markdown", e),
    }
}
