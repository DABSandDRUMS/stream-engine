//! Native defense for the queue browser: no direct watch navigation, and no personal
//! delegate in YouTube API or watch-time requests. The iframe bridge proves the exact channel.
use cef::*;
use se_web::protocol::YoutubeAccount;
use serde_json::Value;
use std::borrow::Cow;
use std::io::{Read, Write};

/// Largest `youtubei` request body accepted, compressed or inflated.
const MAX_BODY: usize = 1024 * 1024;

fn userfree_units(value: &CefStringUserfree) -> &[u16] {
    let raw: Option<&cef::sys::_cef_string_utf16_t> = value.into();
    let Some(raw) = raw else { return &[] };
    if raw.str_.is_null() || raw.length == 0 {
        return &[];
    }
    // SAFETY: CEF owns these units until `value` is dropped; the returned borrow cannot outlive it.
    unsafe { std::slice::from_raw_parts(raw.str_, raw.length) }
}

pub fn inject(frame: &mut Frame, policy: &YoutubeAccount) {
    let url = String::from_utf16_lossy(userfree_units(&frame.url()));
    if !url.starts_with("https://www.youtube.com/embed/?") && url != "https://www.youtube.com/embed/" {
        return;
    }
    let policy_json = serde_json::json!({"channel": policy.channel, "delegate": policy.delegate});
    let script = format!("window.__seYoutubeAccountPolicy = {policy_json};\n{}", include_str!("youtube-account.js"));
    frame.execute_java_script(Some(&script.as_str().into()), Some(&url.as_str().into()), 1);
}

fn valid(policy: &YoutubeAccount) -> bool {
    policy.channel.len() == 24
        && policy.channel.starts_with("UC")
        && policy.channel.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && !policy.delegate.is_empty()
        && policy.delegate.bytes().all(|b| b.is_ascii_digit())
}

fn pin_body(body: &mut Value, delegate: &str) -> bool {
    let Some(context) = body.get_mut("context").and_then(Value::as_object_mut) else { return false };
    let user = context.entry("user").or_insert_with(|| serde_json::json!({}));
    let Some(user) = user.as_object_mut() else { return false };
    if let Some(existing) = user.get("onBehalfOfUser") {
        return existing.as_str() == Some(delegate);
    }
    user.insert("onBehalfOfUser".into(), Value::String(delegate.into()));
    true
}

/// The pinned replacement for a `youtubei` JSON body: `None` rejects, `Some(None)` keeps the
/// body as sent, `Some(Some(bytes))` replaces it. YouTube gzips these bodies
/// (`Content-Encoding: gzip`); they are inflated, pinned, and re-compressed so the header stays true.
fn pinned_body(bytes: &[u8], gzip: bool, delegate: &str) -> Option<Option<Vec<u8>>> {
    let json: Cow<[u8]> = if gzip {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes).take(MAX_BODY as u64 + 1).read_to_end(&mut out).ok()?;
        if out.len() > MAX_BODY {
            return None;
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(bytes)
    };
    let mut body = serde_json::from_slice::<Value>(&json).ok()?;
    let already_pinned = body.pointer("/context/user/onBehalfOfUser").and_then(Value::as_str) == Some(delegate);
    if !pin_body(&mut body, delegate) {
        return None;
    }
    if already_pinned {
        return Some(None);
    }
    let json = serde_json::to_vec(&body).ok()?;
    if !gzip {
        return Some(Some(json));
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(json.len() / 4), flate2::Compression::fast());
    enc.write_all(&json).ok()?;
    Some(Some(enc.finish().ok()?))
}

/// Called on CEF's IO thread, where the request is mutable. False cancels the request.
pub fn allow(request: &mut Request, policy: &YoutubeAccount) -> bool {
    let url = String::from_utf16_lossy(userfree_units(&request.url()));
    // Direct media documents can autoplay before the page gate. Only empty embeds are allowed.
    let Some(path) = url.strip_prefix("https://www.youtube.com/") else {
        return !url.starts_with("https://www.youtube-nocookie.com/");
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path == "watch" || path.starts_with("shorts/") || path.starts_with("live/") || (path.starts_with("embed/") && path != "embed/") {
        return false;
    }
    if !path.starts_with("youtubei/") && !path.starts_with("api/stats/") {
        return true;
    }
    if !valid(policy) {
        return false;
    }
    let header: CefString = "X-Goog-PageId".into();
    let current = request.header_by_name(Some(&header));
    let current = userfree_units(&current);
    if !current.is_empty() && !current.iter().copied().eq(policy.delegate.encode_utf16()) {
        return false;
    }
    request.set_header_by_name(Some(&header), Some(&policy.delegate.as_str().into()), 1);
    if path.starts_with("youtubei/") && userfree_units(&request.method()).iter().copied().eq("POST".encode_utf16()) {
        let encoding: CefString = "Content-Encoding".into();
        let encoding = String::from_utf16_lossy(userfree_units(&request.header_by_name(Some(&encoding))));
        let gzip = match encoding.trim() {
            "" | "identity" => false,
            e if e.eq_ignore_ascii_case("gzip") => true,
            _ => return false,
        };
        let Some(post) = request.post_data() else { return false };
        if post.element_count() != 1 {
            return false;
        }
        let mut elements = vec![None];
        post.elements(Some(&mut elements));
        let Some(element) = elements.into_iter().next().flatten() else { return false };
        if element.get_type() != PostdataelementType::BYTES {
            return false;
        }
        let len = element.bytes_count();
        if len == 0 || len > MAX_BODY {
            return false;
        }
        let mut bytes = vec![0; len];
        if element.bytes(len, bytes.as_mut_ptr()) != len {
            return false;
        }
        match pinned_body(&bytes, gzip, &policy.delegate) {
            None => return false,
            Some(Some(bytes)) => element.set_to_bytes(bytes.len(), bytes.as_ptr()),
            Some(None) => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_missing_delegate_and_reject_conflicting_identity() {
        let mut body = serde_json::json!({"context": {"client": {"clientName": "WEB"}}, "videoId": "queued"});
        assert!(pin_body(&mut body, "123456"));
        assert_eq!(body["context"]["user"]["onBehalfOfUser"], "123456");
        assert!(pin_body(&mut body, "123456"));
        assert!(!pin_body(&mut body, "987654"));
        assert_eq!(body["context"]["user"]["onBehalfOfUser"], "123456");
        assert!(!pin_body(&mut serde_json::json!({"context": {"user": null}}), "123456"));
        assert!(!pin_body(&mut serde_json::json!({}), "123456"));
    }

    #[test]
    fn gzip_bodies_are_pinned_and_recompressed() {
        let gz = |v: &Value| {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&serde_json::to_vec(v).unwrap()).unwrap();
            enc.finish().unwrap()
        };
        let inflate = |b: &[u8]| -> Value {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(b).read_to_end(&mut out).unwrap();
            serde_json::from_slice(&out).unwrap()
        };
        let sent = gz(&serde_json::json!({"context": {"client": {"clientName": "WEB_EMBEDDED_PLAYER"}}, "videoId": "ntDamUOM7f4"}));
        let pinned = pinned_body(&sent, true, "123456").unwrap().expect("pinned body replaces the original");
        let body = inflate(&pinned);
        assert_eq!(body["context"]["user"]["onBehalfOfUser"], "123456");
        assert_eq!(body["videoId"], "ntDamUOM7f4");
        assert_eq!(pinned_body(&pinned, true, "123456"), Some(None), "already pinned bodies pass unchanged");
        let other = gz(&serde_json::json!({"context": {"user": {"onBehalfOfUser": "987654"}}}));
        assert_eq!(pinned_body(&other, true, "123456"), None, "conflicting identity is rejected");
        assert_eq!(pinned_body(&sent, false, "123456"), None, "gzip bytes without the header are not JSON");
        let bomb = gz(&serde_json::json!({"context": {}, "pad": "x".repeat(MAX_BODY)}));
        assert_eq!(pinned_body(&bomb, true, "123456"), None, "inflated size is bounded");
    }
}
