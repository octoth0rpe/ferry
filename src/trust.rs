//! The page for trusting ferry's local CA: the certificate, and how to trust
//! it on each platform.
//!
//! It has a plain HTTP port of its own, picked by the OS: plain HTTP because
//! a device that doesn't trust the CA yet can't fetch anything over HTTPS
//! without a warning, and its own port so ferry never serves anything of its
//! own at an app's address.

use std::path::PathBuf;

use bytes::Bytes;
use http::header::{ALLOW, CONTENT_DISPOSITION, CONTENT_TYPE};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};

use crate::ca::LocalCa;
use crate::dirs;
use crate::proxy::{error_page, escape_html};
use crate::upstream::Body;

/// The page and certificate, rendered once per configuration.
pub struct Page {
    html: Bytes,
    der: Bytes,
}

impl Page {
    pub fn new(ca: &LocalCa) -> Page {
        Page {
            html: Bytes::from(render(ca)),
            der: Bytes::from(ca.root_der.clone()),
        }
    }

    /// Answers a request to the page's port.
    pub fn respond<B>(&self, req: &Request<B>) -> Response<Body> {
        if req.method() != Method::GET && req.method() != Method::HEAD {
            let mut resp = error_page(
                StatusCode::METHOD_NOT_ALLOWED,
                "Only GET is allowed.",
                false,
            );
            resp.headers_mut()
                .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
            return resp;
        }
        let html = |body: &Bytes| response(body.clone(), "text/html; charset=utf-8");
        let cert = |attachment: bool| {
            let mut resp = response(self.der.clone(), "application/x-x509-ca-cert");
            if attachment {
                // Android installs CA certificates from Settings, not from
                // the browser, so the file needs to land in Downloads.
                resp.headers_mut().insert(
                    CONTENT_DISPOSITION,
                    HeaderValue::from_static("attachment; filename=\"ferry-ca.crt\""),
                );
            }
            resp
        };
        match req.uri().path() {
            "/" => html(&self.html),
            "/ca.crt" => cert(false),
            "/ferry-ca.crt" => cert(true),
            _ => error_page(StatusCode::NOT_FOUND, "Not found.", true),
        }
    }
}

fn response(body: Bytes, content_type: &'static str) -> Response<Body> {
    let mut resp = Response::new(Full::new(body).map_err(|never| match never {}).boxed());
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

/// Commands that make this machine trust the root, best first, each with a
/// short description.
pub fn desktop_commands(ca: &LocalCa) -> Vec<(String, String)> {
    let pem = sh_quote(&ca.paths.root.cert.display().to_string());
    let mut commands = Vec::new();
    if cfg!(target_os = "macos") {
        commands.push((
            "macOS keychain (Safari, Chrome, and Firefox with enterprise roots on)".into(),
            format!(
                "security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db {pem}"
            ),
        ));
    } else {
        commands.push((
            "Debian, Ubuntu".into(),
            format!(
                "sudo cp {pem} /usr/local/share/ca-certificates/ferry.crt && sudo update-ca-certificates"
            ),
        ));
        commands.push((
            "Fedora, Arch, openSUSE".into(),
            format!("sudo trust anchor --store {pem}"),
        ));
    }
    for profile in nss_profiles() {
        let what = match profile.ends_with(".pki/nssdb") {
            true => "Chrome and Chromium".into(),
            false => format!(
                "Firefox profile {}",
                profile.file_name().unwrap_or_default().to_string_lossy()
            ),
        };
        commands.push((
            what,
            format!(
                "certutil -d sql:{} -A -t C,, -n {} -i {pem}",
                sh_quote(&profile.display().to_string()),
                sh_quote(&ca.root_name)
            ),
        ));
    }
    commands
}

/// NSS certificate databases that browsers on this machine use instead of
/// the system store: Firefox profiles, and Chrome's on Linux.
fn nss_profiles() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let firefox: &[&str] = if cfg!(target_os = "macos") {
        &["Library/Application Support/Firefox/Profiles"]
    } else {
        &[
            ".mozilla/firefox",
            "snap/firefox/common/.mozilla/firefox",
            ".var/app/org.mozilla.firefox/.mozilla/firefox",
        ]
    };
    let mut found: Vec<PathBuf> = firefox
        .iter()
        .filter_map(|dir| std::fs::read_dir(home.join(dir)).ok())
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    if !cfg!(target_os = "macos") {
        found.push(home.join(".pki/nssdb"));
    }
    found.retain(|dir| dir.join("cert9.db").is_file());
    found.sort();
    found
}

/// Quotes `s` for a POSIX shell, unless it doesn't need it.
fn sh_quote(s: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+,:@~".contains(c);
    if !s.is_empty() && s.chars().all(plain) && !s.starts_with('~') {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The fingerprint in two halves, which fit on a phone screen and in an
/// 80-column terminal.
pub fn fingerprint_lines(fingerprint: &str) -> [&str; 2] {
    let mid = fingerprint.len() / 2;
    // 32 bytes as "AB:" groups: the 16th colon is at index 47.
    let split = fingerprint[mid..].find(':').map_or(mid, |i| mid + i);
    [
        &fingerprint[..split],
        fingerprint[split..].trim_start_matches(':'),
    ]
}

fn render(ca: &LocalCa) -> String {
    let name = escape_html(&ca.root_name);
    let [fp1, fp2] = fingerprint_lines(&ca.fingerprint);
    let commands: String = desktop_commands(ca)
        .iter()
        .map(|(what, cmd)| {
            format!(
                "<p>{}:</p>\n<pre>{}</pre>\n",
                escape_html(what),
                escape_html(cmd)
            )
        })
        .collect();
    let certutil_note = if cfg!(target_os = "macos") {
        "Firefox can use the keychain instead: set <code>security.enterprise_roots.enabled</code> \
         to <code>true</code> in <code>about:config</code>. Or add the CA to each profile with \
         <code>certutil</code> (<code>brew install nss</code>)."
    } else {
        "Firefox, and Chrome on Linux, keep their own certificate stores. Add the CA to each \
         with <code>certutil</code> (in <code>libnss3-tools</code> or <code>nss-tools</code>)."
    };
    format!(
        r#"<!doctype html>
<meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>Trust ferry's local CA</title>
<style>
body {{ font: 16px/1.5 system-ui, sans-serif; max-width: 40em; margin: 2em auto; padding: 0 1em; }}
pre, code {{ font: 14px ui-monospace, monospace; }}
pre {{ background: #f2f2f2; padding: .5em .75em; overflow-x: auto; white-space: pre-wrap; word-break: break-all; }}
a.button {{ display: inline-block; padding: .5em 1em; background: #0a6cff; color: #fff; text-decoration: none; border-radius: .4em; }}
li {{ margin: .4em 0; }}
@media (prefers-color-scheme: dark) {{
  body {{ background: #111; color: #eee; }}
  pre {{ background: #222; }}
  a {{ color: #6aa8ff; }}
}}
</style>
<h1>Trust ferry's local CA</h1>
<p>ferry signs its HTTPS certificates with <b>{name}</b>. Trust it once on a
device and every ferry on this computer works there without warnings,
service workers included.</p>
<p>Check this fingerprint (SHA-256) matches the one ferry printed when it
started. This page came over plain HTTP, so on a shared network someone could
have swapped the certificate.</p>
<pre>{fp1}
{fp2}</pre>

<h2>iPhone and iPad</h2>
<ol>
<li>Open this page in <b>Safari</b>. Other browsers can't install
certificates.</li>
<li><a class=button href="/ca.crt">Download the certificate</a>, then
tap <b>Allow</b>.</li>
<li>Open <b>Settings → General → VPN &amp; Device Management</b>, tap
<b>{name}</b> under Downloaded Profile, then <b>Install</b>. Do this within
8 minutes: iOS discards a downloaded profile that isn't installed.</li>
<li>Go to <b>Settings → General → About → Certificate Trust Settings</b> and
turn on <b>{name}</b>.</li>
</ol>

<h2>Android</h2>
<ol>
<li><a class=button href="/ferry-ca.crt">Download the certificate</a>.</li>
<li>Open <b>Settings</b> and search for <b>CA certificate</b> (usually under
Security → Encryption &amp; credentials → Install a certificate).</li>
<li>Tap <b>Install anyway</b> and pick <b>ferry-ca.crt</b> from Downloads.</li>
</ol>
<p>Android then shows "Network may be monitored". That's expected for any
certificate you add yourself.</p>

<h2>This computer</h2>
{commands}<p>{certutil_note}</p>
<p>Restart the browser afterwards.</p>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(sh_quote("/home/me/ca.pem"), "/home/me/ca.pem");
        assert_eq!(
            sh_quote("/Users/me/Library/Application Support/ferry/ca.pem"),
            "'/Users/me/Library/Application Support/ferry/ca.pem'"
        );
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("~x"), "'~x'");
    }

    #[test]
    fn splits_fingerprints() {
        let fp = crate::ca::fingerprint(b"anything");
        let [a, b] = fingerprint_lines(&fp);
        assert_eq!((a.len(), b.len()), (47, 47));
        assert_eq!(format!("{a}:{b}"), fp);
    }
}
