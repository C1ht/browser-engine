//! Resource loading and URL resolution.
//!
//! file:// is read directly. http(s):// is delegated to the system `curl`
//! (bundled with Windows 10+, macOS and most Linux distros) so we get TLS,
//! HTTP/2, redirects and compression without pulling in a networking stack.

use std::process::Command;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) bolt/0.1 (like Gecko)";

/// Where cookies persist between requests and runs.
fn cookie_jar() -> String {
    std::env::temp_dir().join("bolt-cookies.txt").to_string_lossy().into_owned()
}

pub fn fetch(url: &str) -> Result<(String, Vec<u8>), String> {
    request(url, None)
}

/// Fetch a URL, POSTing `post` (form-urlencoded) if given. Returns the final
/// URL after redirects and the body. Cookies are read from and saved to the jar.
pub fn request(url: &str, post: Option<&str>) -> Result<(String, Vec<u8>), String> {
    if url.starts_with("file:") {
        let path = file_url_to_path(url);
        return std::fs::read(&path).map(|b| (url.to_owned(), b)).map_err(|e| format!("{path}: {e}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let jar = cookie_jar();
        let mut cmd = Command::new("curl");
        cmd.args(["-sSL", "--compressed", "--max-time", "20", "-A", UA, "-b", &jar, "-c", &jar, "-w", "\n%{url_effective}"]);
        if let Some(body) = post {
            // curl sends the form Content-Type itself (setting it by hand
            // reorders headers, which some sites treat as a bot signal).
            cmd.args(["--data-binary", body]);
        }
        let out = cmd
            .arg(url)
            .output()
            .map_err(|e| format!("could not run curl: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        let mut body = out.stdout;
        let nl = body.iter().rposition(|&b| b == b'\n').unwrap_or(body.len());
        let final_url = String::from_utf8_lossy(&body[(nl + 1).min(body.len())..]).trim().to_owned();
        body.truncate(nl);
        let final_url = if final_url.is_empty() { url.to_owned() } else { final_url };
        return Ok((final_url, body));
    }
    Err(format!("unsupported URL scheme: {url}"))
}

/// Fetch many URLs at once. HTTP(S) URLs go through a single curl process
/// with `--parallel`, so connections (and HTTP/2 multiplexing) are reused
/// instead of paying a TLS handshake per resource.
pub fn fetch_many(urls: &[String]) -> Vec<Option<Vec<u8>>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BATCH: AtomicUsize = AtomicUsize::new(0);
    let mut out: Vec<Option<Vec<u8>>> = vec![None; urls.len()];
    let mut remote = Vec::new();
    for (i, u) in urls.iter().enumerate() {
        if u.starts_with("http://") || u.starts_with("https://") {
            remote.push(i);
        } else if u.starts_with("data:") {
            out[i] = crate::image::data_url(u);
        } else {
            out[i] = fetch(u).ok().map(|(_, b)| b);
        }
    }
    if remote.is_empty() {
        return out;
    }
    let dir = std::env::temp_dir().join(format!("bolt-{}-{}", std::process::id(), BATCH.fetch_add(1, Ordering::Relaxed)));
    if std::fs::create_dir_all(&dir).is_err() {
        return out;
    }
    let esc = |s: &str| s.replace('\\', "/").replace('"', "%22");
    let mut cfg = String::new();
    for &i in &remote {
        let path = dir.join(i.to_string());
        cfg.push_str(&format!("url = \"{}\"\noutput = \"{}\"\n", esc(&urls[i]), esc(&path.to_string_lossy())));
    }
    let cfg_path = dir.join("batch.cfg");
    if std::fs::write(&cfg_path, cfg).is_ok() {
        let _ = Command::new("curl")
            .args(["-sSL", "--fail", "--compressed", "--max-time", "30", "--parallel", "--parallel-max", "16", "-A", UA, "-b", &cookie_jar(), "-K"])
            .arg(&cfg_path)
            .output();
        for &i in &remote {
            out[i] = std::fs::read(dir.join(i.to_string())).ok();
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// Decode bytes to text, honouring a Latin-1 / Windows-1252 charset declaration.
pub fn decode(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(2048)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    let latin = head
        .find("charset=")
        .map(|p| {
            let cs = head[p + 8..].trim_start_matches(['"', '\'']);
            cs.starts_with("iso-8859-1") || cs.starts_with("windows-1252") || cs.starts_with("latin1")
        })
        .unwrap_or(false);
    if latin && std::str::from_utf8(bytes).is_err() {
        bytes.iter().map(|&b| b as char).collect()
    } else {
        match String::from_utf8(bytes.to_vec()) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn file_url_to_path(url: &str) -> String {
    let p = url.trim_start_matches("file:").trim_start_matches("//");
    let p = p.split(['?', '#']).next().unwrap_or("");
    let p = percent_decode(p);
    // file:///C:/x -> C:/x on Windows
    let b = p.as_bytes();
    if b.len() > 2 && b[0] == b'/' && b[2] == b':' {
        p[1..].to_owned()
    } else {
        p
    }
}

pub fn path_to_file_url(path: &str) -> String {
    let abs = std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_owned());
    let abs = abs.trim_start_matches(r"\\?\").replace('\\', "/").replace(' ', "%20");
    if abs.starts_with('/') { format!("file://{abs}") } else { format!("file:///{abs}") }
}

/// Turn address-bar input into a URL.
pub fn normalize_input(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("about:") || s.contains("://") || s.starts_with("file:") {
        return s.to_owned();
    }
    if std::path::Path::new(s).exists() {
        return path_to_file_url(s);
    }
    if s.contains('.') && !s.contains(' ') {
        return format!("https://{s}");
    }
    format!("https://html.duckduckgo.com/html/?q={}", crate::forms::urlencode(s))
}

fn has_scheme(s: &str) -> bool {
    match s.find(':') {
        Some(p) if p > 1 => s[..p].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'),
        _ => false,
    }
}

/// Resolve `href` against `base` (RFC 3986, simplified).
pub fn resolve(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.is_empty() {
        return base.to_owned();
    }
    if has_scheme(href) {
        return href.to_owned();
    }
    let scheme_end = base.find("://").map(|p| p + 3).unwrap_or(0);
    let scheme = &base[..scheme_end.saturating_sub(3)];
    if let Some(r) = href.strip_prefix("//") {
        return format!("{scheme}://{r}");
    }
    let path_start = base[scheme_end..].find('/').map(|p| p + scheme_end).unwrap_or(base.len());
    let origin = &base[..path_start];
    let base_path = &base[path_start..];
    let base_path_noq = base_path.split(['?', '#']).next().unwrap_or("");
    if href.starts_with('#') {
        let b = base.split('#').next().unwrap_or(base);
        return format!("{b}{href}");
    }
    if href.starts_with('?') {
        return format!("{origin}{base_path_noq}{href}");
    }
    let (hpath, suffix) = match href.find(['?', '#']) {
        Some(p) => (&href[..p], &href[p..]),
        None => (href, ""),
    };
    let joined = if hpath.starts_with('/') {
        hpath.to_owned()
    } else {
        let dir = match base_path_noq.rfind('/') {
            Some(p) => &base_path_noq[..=p],
            None => "/",
        };
        format!("{dir}{hpath}")
    };
    let mut segs: Vec<&str> = Vec::new();
    let parts: Vec<&str> = joined.split('/').collect();
    for (i, seg) in parts.iter().enumerate() {
        match *seg {
            "." => {
                if i == parts.len() - 1 {
                    segs.push("");
                }
            }
            ".." => {
                if segs.len() > 1 {
                    segs.pop();
                }
                if i == parts.len() - 1 {
                    segs.push("");
                }
            }
            s => segs.push(s),
        }
    }
    let mut path = segs.join("/");
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    format!("{origin}{path}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolving() {
        let b = "https://ex.com/a/b/c.html?x=1#f";
        assert_eq!(resolve(b, "d.html"), "https://ex.com/a/b/d.html");
        assert_eq!(resolve(b, "../d.html"), "https://ex.com/a/d.html");
        assert_eq!(resolve(b, "/z"), "https://ex.com/z");
        assert_eq!(resolve(b, "//cdn.com/x.css"), "https://cdn.com/x.css");
        assert_eq!(resolve(b, "?q=2"), "https://ex.com/a/b/c.html?q=2");
        assert_eq!(resolve(b, "#top"), "https://ex.com/a/b/c.html?x=1#top");
        assert_eq!(resolve("https://ex.com", "a"), "https://ex.com/a");
        assert_eq!(resolve("file:///C:/dir/x.html", "y.css"), "file:///C:/dir/y.css");
        assert_eq!(file_url_to_path("file:///C:/my%20dir/x.html"), "C:/my dir/x.html");
    }
}
