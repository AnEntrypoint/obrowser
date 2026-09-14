#![cfg(not(feature = "native"))]

use crate::browser::BrowserId;
use crate::config::BrowserConfig;
use crate::network::HttpClient;
use crate::network::cookie::CookieJar;
use crate::session::Session;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::alloc::{Layout, alloc, dealloc};
use std::cell::RefCell;
use std::collections::HashMap;
use std::mem;
use std::sync::Arc;

pub const PAGE_FIELD: &str = "page";
pub const DEFAULT_PAGE: &str = "default";
pub const MAX_PAGES: usize = 16;

struct PageSlot {
    session: Session,
    last_used: u64,
}

#[derive(Default)]
struct PageTable {
    slots: HashMap<String, PageSlot>,
    clock: u64,
}

impl PageTable {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn evict_least_recently_used_beyond_capacity(&mut self, keep: &str) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.slots.len() > MAX_PAGES {
            let oldest = self
                .slots
                .iter()
                .filter(|(key, _)| key.as_str() != keep)
                .min_by(|(ka, a), (kb, b)| a.last_used.cmp(&b.last_used).then_with(|| ka.cmp(kb)))
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else { break };
            self.slots.remove(&oldest);
            evicted.push(oldest);
        }
        evicted
    }
}

thread_local! {
    static PAGES: RefCell<PageTable> = RefCell::new(PageTable::default());
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

fn ok(verb: &str, page: &str, data: Value) -> u64 {
    return_bytes(
        json!({ "ok": true, "verb": verb, "page": page, "data": data })
            .to_string()
            .into_bytes(),
    )
}

fn err(verb: &str, page: &str, message: impl Into<String>) -> u64 {
    return_bytes(
        json!({ "ok": false, "verb": verb, "page": page, "error": message.into() })
            .to_string()
            .into_bytes(),
    )
}

fn page_of(body: &Value) -> String {
    body.get(PAGE_FIELD)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|page| !page.is_empty())
        .unwrap_or(DEFAULT_PAGE)
        .to_string()
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_call(verb_ptr: u32, verb_len: u32, body_ptr: u32, body_len: u32) -> u64 {
    let verb = read_str(verb_ptr, verb_len);
    let body_s = read_str(body_ptr, body_len);
    let body: Value = if body_s.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body_s).unwrap_or(Value::Null)
    };
    let page = page_of(&body);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch_verb_inner(&verb, &page, &body)
    }));
    match result {
        Ok(packed) => packed,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic during dispatch".to_string());
            err(&verb, &page, format!("panicked: {msg}"))
        }
    }
}

fn dispatch_verb_inner(verb: &str, page: &str, body: &Value) -> u64 {
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(e) => return err(verb, page, format!("failed to build async runtime: {e}")),
    };
    rt.block_on(async { dispatch_verb_async(verb, page, body).await })
}

async fn dispatch_verb_async(verb: &str, page: &str, body: &Value) -> u64 {
    match verb {
        "navigate" => verb_navigate(page, body).await,
        "evaluate" => verb_evaluate(page, body).await,
        "dom-query" => verb_dom_query(page, body).await,
        "extract-markdown" => verb_extract_markdown(page).await,
        "list-pages" => verb_list_pages(page),
        "close-page" => verb_close_page(page),
        "capabilities" => ok(
            "capabilities",
            page,
            json!({
                "plugin": "oxibrowser",
                "verbs": ["navigate", "evaluate", "dom-query", "extract-markdown", "list-pages", "close-page", "capabilities"],
                "page_field": PAGE_FIELD,
                "default_page": DEFAULT_PAGE,
                "max_pages": MAX_PAGES,
                "payload_field": {
                    "navigate": "navigated",
                    "evaluate": "value",
                    "dom-query": "nodes",
                    "extract-markdown": "markdown",
                    "list-pages": "pages",
                    "close-page": "closed",
                },
            }),
        ),
        other => err(other, page, format!("unknown verb: {other}")),
    }
}

async fn open_standalone_session() -> Result<Session, String> {
    let config = BrowserConfig::headless();
    let cookie_jar = Arc::new(RwLock::new(CookieJar::new()));
    let http_client =
        Arc::new(HttpClient::new(&config, cookie_jar.clone()).map_err(|e| e.to_string())?);
    Session::new(BrowserId::next(), config, http_client, cookie_jar)
        .await
        .map_err(|e| e.to_string())
}

async fn with_page<F, T>(page: &str, f: F) -> Result<T, String>
where
    F: for<'a> FnOnce(
        &'a mut Session,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::error::Result<T>> + 'a>,
    >,
{
    let parked = PAGES.with_borrow_mut(|table| table.slots.remove(page));
    let mut session = match parked {
        Some(slot) => slot.session,
        None => open_standalone_session().await?,
    };
    let result = f(&mut session).await.map_err(|e| e.to_string());
    PAGES.with_borrow_mut(|table| {
        let last_used = table.tick();
        table.slots.insert(page.to_string(), PageSlot { session, last_used });
        table.evict_least_recently_used_beyond_capacity(page);
    });
    result
}

async fn verb_navigate(page: &str, body: &Value) -> u64 {
    let Some(url) = body.get("url").and_then(|v| v.as_str()) else {
        return err("navigate", page, "missing required field: url");
    };
    let url = url.to_string();
    match with_page(page, |session| Box::pin(async move { session.navigate(&url).await })).await {
        Ok(()) => ok("navigate", page, json!({ "navigated": true })),
        Err(e) => err("navigate", page, e),
    }
}

async fn verb_evaluate(page: &str, body: &Value) -> u64 {
    let Some(expression) = body.get("expression").and_then(|v| v.as_str()) else {
        return err("evaluate", page, "missing required field: expression");
    };
    let expression = expression.to_string();
    match with_page(page, |session| Box::pin(async move { session.evaluate_js(&expression).await }))
        .await
    {
        Ok(result) => ok(
            "evaluate",
            page,
            json!({ "value": result.value, "exception": result.exception }),
        ),
        Err(e) => err("evaluate", page, e),
    }
}

async fn verb_dom_query(page: &str, body: &Value) -> u64 {
    let Some(selector) = body.get("selector").and_then(|v| v.as_str()) else {
        return err("dom-query", page, "missing required field: selector");
    };
    let selector = selector.to_string();
    match with_page(page, |session| Box::pin(async move { session.dom_snapshot().await })).await {
        Ok(Some(snapshot)) => {
            let matched = snapshot.query_selector_all(&selector);
            let nodes: Vec<Value> = matched
                .into_iter()
                .filter_map(|id| snapshot.nodes.get(&id))
                .map(|n| json!({ "tag": n.tag, "text": n.text_content }))
                .collect();
            ok("dom-query", page, json!({ "nodes": nodes }))
        }
        Ok(None) => ok("dom-query", page, json!({ "nodes": [] })),
        Err(e) => err("dom-query", page, e),
    }
}

async fn verb_extract_markdown(page: &str) -> u64 {
    match with_page(page, |session| {
        Box::pin(async move { Ok(session.page().map(|p| p.to_markdown()).unwrap_or_default()) })
    })
    .await
    {
        Ok(markdown) => ok("extract-markdown", page, json!({ "markdown": markdown })),
        Err(e) => err("extract-markdown", page, e),
    }
}

fn verb_list_pages(page: &str) -> u64 {
    let pages: Vec<Value> = PAGES.with_borrow(|table| {
        let mut slots: Vec<(&String, &PageSlot)> = table.slots.iter().collect();
        slots.sort_by(|(ka, a), (kb, b)| b.last_used.cmp(&a.last_used).then_with(|| ka.cmp(kb)));
        slots
            .into_iter()
            .map(|(key, slot)| {
                json!({
                    "page": key,
                    "url": slot.session.current_url().map(|u| u.to_string()),
                    "last_used": slot.last_used,
                })
            })
            .collect()
    });
    ok("list-pages", page, json!({ "pages": pages, "max_pages": MAX_PAGES }))
}

fn verb_close_page(page: &str) -> u64 {
    let closed = PAGES.with_borrow_mut(|table| table.slots.remove(page).is_some());
    ok("close-page", page, json!({ "closed": closed }))
}
