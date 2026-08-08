//! wasm-only host ABI: network calls a wasm guest cannot make itself (no
//! native TLS/socket stack under `wasm32-wasip1`) are proxied to the JS host
//! runtime (gm's agentplug) via imported WASI functions. Mirrors the
//! packed-`u64` ptr+len ABI used by gm's own `plugkit-core` wasm_dispatch
//! (`host_fetch`, pack/unpack helpers) so the same host runtime can serve
//! both plugins without a second calling convention.
#![cfg(not(feature = "native"))]

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// `host_fetch(url_ptr, url_len, opts_ptr, opts_len) -> packed(ptr,len)`
    /// of a JSON response `{"status":u16,"headers":[[k,v],..],"body":"<base64>"}`
    /// or `{"error":"<message>"}` on failure. `opts` is a JSON object
    /// `{"method":"GET","headers":{...},"body":"<base64>"}`.
    fn host_fetch(url_ptr: *const u8, url_len: u32, opts_ptr: *const u8, opts_len: u32) -> u64;
}

fn pack_ptr_len(ptr: usize, len: usize) -> u64 {
    assert!(
        (ptr as u64) <= 0xffff_ffff,
        "pack: pointer {ptr:#x} exceeds the 32-bit field of the packed ABI"
    );
    assert!(
        (len as u64) <= 0xffff_ffff,
        "pack: length {len} exceeds the 32-bit field of the packed ABI"
    );
    (ptr as u64 & 0xffff_ffff) | ((len as u64) << 32)
}

fn unpack_to_string(packed: u64) -> Option<String> {
    let p = (packed & 0xffff_ffff) as u32;
    let l = (packed >> 32) as u32;
    if p == 0 || l == 0 {
        return None;
    }
    let bytes = unsafe { Vec::from_raw_parts(p as *mut u8, l as usize, l as usize) };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn unpack_to_value(packed: u64) -> serde_json::Value {
    match unpack_to_string(packed) {
        Some(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s)),
        None => serde_json::Value::Null,
    }
}

/// A host-proxied HTTP response, decoded from `host_fetch`'s JSON envelope.
pub struct HostFetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Fetch `url` via the host's network stack. `method`/`headers`/`body` mirror
/// a plain HTTP request; the host performs the real TLS/socket work and
/// returns the decoded response (or an error string) as JSON.
pub fn host_fetch_call(
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<HostFetchResponse, String> {
    use base64::Engine;
    let opts = serde_json::json!({
        "method": method,
        "headers": headers.iter().cloned().collect::<std::collections::HashMap<_, _>>(),
        "body": body.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
    });
    let opts_s = opts.to_string();
    let packed = unsafe {
        host_fetch(
            url.as_ptr(),
            url.len() as u32,
            opts_s.as_ptr(),
            opts_s.len() as u32,
        )
    };
    let v = unpack_to_value(packed);
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16;
    let headers = v
        .get("headers")
        .and_then(|h| h.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    Some((pair.first()?.as_str()?.to_string(), pair.get(1)?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let body = v
        .get("body")
        .and_then(|b| b.as_str())
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .unwrap_or_default();
    Ok(HostFetchResponse {
        status,
        headers,
        body,
    })
}

// Silence "unused ptr helper" for now — pack_ptr_len is part of the shared
// ABI contract even though this file's current calls only ever unpack.
#[allow(dead_code)]
fn _abi_contract_uses_pack(ptr: usize, len: usize) -> u64 {
    pack_ptr_len(ptr, len)
}
