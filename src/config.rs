//! Config file loading and the resolved runtime configuration.
//!
//! The effective configuration is built from an optional TOML file plus
//! command-line arguments. CLI flags override file values; CLI mappings are
//! added to the file's `[[proxy]]` entries.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use toml::Spanned;

use crate::cli::Cli;
use crate::mapping::{self, Mapping, Upstream};

/// A certificate chain and its private key, both PEM files.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CertKey {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Certificate for listeners without their own; `None` means ferry's
    /// local CA.
    pub cert_key: Option<CertKey>,
    pub listeners: Vec<Listener>,
    pub access_log: AccessLog,
    pub error_log: ErrorLog,
    pub log_format: LogFormat,
    pub quiet: bool,
    /// Default request body limit in bytes; `None` means unlimited.
    pub max_body: Option<u64>,
    pub redirect: bool,
    /// Where to write the process id, if anywhere.
    pub pid_file: Option<PathBuf>,
    pub advanced: Advanced,
    /// Absolute path of the config file, if one was used.
    pub config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Listener {
    pub mapping: Mapping,
    /// Overrides [`Config::cert_key`] for this listener.
    pub cert_key: Option<CertKey>,
    /// Overrides [`Config::max_body`] for this listener.
    pub max_body: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessLog {
    Stdout,
    Off,
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorLog {
    Stderr,
    File(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Common,
    #[default]
    Combined,
    Json,
}

/// Settings almost nobody needs to change. Config file only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    /// Time allowed to connect to an upstream.
    pub connect_timeout: Duration,
    /// Time allowed for a client's TLS handshake and first request.
    pub handshake_timeout: Duration,
    /// How long a connection is kept with no requests, and how long a
    /// request may go with nothing moving.
    pub idle_timeout: Duration,
    /// How long in-flight requests get to finish on shutdown.
    pub shutdown_grace: Duration,
    /// Show QR codes in the banner for phones: the local CA, and each app.
    pub qr_code: bool,
    /// Give a new local CA name constraints, so it can only vouch for local
    /// names and private addresses.
    pub ca_name_constraints: bool,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced {
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(90),
            shutdown_grace: Duration::from_secs(10),
            qr_code: true,
            ca_name_constraints: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cert_key: None,
            listeners: Vec::new(),
            access_log: AccessLog::Stdout,
            error_log: ErrorLog::Stderr,
            log_format: LogFormat::default(),
            quiet: false,
            max_body: None,
            redirect: true,
            pid_file: None,
            advanced: Advanced::default(),
            config_path: None,
        }
    }
}

impl Listener {
    /// The certificate this listener serves; `None` means ferry's local CA.
    pub fn cert_key<'a>(&'a self, config: &'a Config) -> Option<&'a CertKey> {
        self.cert_key.as_ref().or(config.cert_key.as_ref())
    }

    pub fn max_body(&self, config: &Config) -> Option<u64> {
        self.max_body.or(config.max_body)
    }
}

impl Config {
    /// Builds the effective configuration from the command line and, with
    /// `-c`, the config file. Returns every problem found, not just the first.
    #[cfg(test)]
    pub fn load(cli: &Cli) -> Result<Config, Vec<String>> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Config::load_in(cli, &cwd)
    }

    /// Like [`Config::load`], resolving relative command-line paths against
    /// `cwd` rather than the current directory (which a daemon changes).
    pub fn load_in(cli: &Cli, cwd: &Path) -> Result<Config, Vec<String>> {
        let positionals = cli.positionals().map_err(|e| vec![e])?;
        let mut config = match &cli.config {
            Some(path) => load_file(&cwd.join(path))?,
            None => Config::default(),
        };
        let mut errors = Vec::new();

        if let Some(ck) = positionals.cert_key {
            config.cert_key = Some(CertKey {
                cert: absolute(&cwd.join(&ck.cert)),
                key: absolute(&cwd.join(&ck.key)),
            });
        }
        for mut mapping in positionals.mappings {
            if let Upstream::Unix(path) = &mut mapping.upstream {
                *path = absolute(&cwd.join(&path));
            }
            config.listeners.push(Listener {
                mapping,
                cert_key: None,
                max_body: None,
            });
        }
        if let Some(s) = &cli.access_log {
            config.access_log = AccessLog::parse(s, cwd);
        }
        if let Some(s) = &cli.error_log {
            match ErrorLog::parse(s, cwd) {
                Ok(log) => config.error_log = log,
                Err(e) => errors.push(e),
            }
        }
        if let Some(format) = cli.log_format {
            config.log_format = format;
        }
        if let Some(s) = &cli.max_body {
            match parse_size(s) {
                Ok(n) => config.max_body = Some(n),
                Err(e) => errors.push(format!("--max-body: {e}")),
            }
        }
        if let Some(p) = &cli.pid_file {
            config.pid_file = Some(absolute(&cwd.join(p)));
        }
        config.quiet = cli.quiet;
        if cli.no_redirect {
            config.redirect = false;
        }

        for (i, a) in config.listeners.iter().enumerate() {
            if let Some((b, port)) = config.listeners[..i]
                .iter()
                .find_map(|b| Some((b, b.mapping.conflict(&a.mapping)?)))
            {
                errors.push(format!(
                    "`{}` and `{}` both listen on port {port}",
                    b.mapping.listen_display(),
                    a.mapping.listen_display(),
                ));
            }
        }
        if config.listeners.is_empty() && errors.is_empty() {
            errors.push(match &cli.config {
                Some(_) => {
                    "nothing to proxy; add a [[proxy]] section or a mapping like 6005:6006".into()
                }
                None => "nothing to proxy; give at least one mapping, e.g. 6005:6006".into(),
            });
        }

        if errors.is_empty() {
            Ok(config)
        } else {
            Err(errors)
        }
    }
}

impl AccessLog {
    fn parse(s: &str, base: &Path) -> AccessLog {
        match s {
            "-" | "stdout" => AccessLog::Stdout,
            "off" => AccessLog::Off,
            path => AccessLog::File(absolute(&base.join(path))),
        }
    }
}

impl ErrorLog {
    fn parse(s: &str, base: &Path) -> Result<ErrorLog, String> {
        match s {
            "-" | "stderr" => Ok(ErrorLog::Stderr),
            "off" => Err("the error log can't be turned off".into()),
            path => Ok(ErrorLog::File(absolute(&base.join(path)))),
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LogFormat::Common => "common",
            LogFormat::Combined => "combined",
            LogFormat::Json => "json",
        })
    }
}

impl Config {
    /// The configuration as a TOML config file that loads back to the same
    /// thing (`ferry --print-config ... > ferry.toml`). Advanced settings
    /// are only included where they differ from the defaults.
    pub fn to_toml(&self) -> String {
        let path = |p: &Path| p.display().to_string();
        let (cert, key) = cert_key_out(self.cert_key.as_ref());
        let defaults = Advanced::default();
        let changed = |v: Duration, d: Duration| (v != d).then(|| format_duration(v));
        let advanced = OutAdvanced {
            connect_timeout: changed(self.advanced.connect_timeout, defaults.connect_timeout),
            handshake_timeout: changed(self.advanced.handshake_timeout, defaults.handshake_timeout),
            idle_timeout: changed(self.advanced.idle_timeout, defaults.idle_timeout),
            shutdown_grace: changed(self.advanced.shutdown_grace, defaults.shutdown_grace),
            qr_code: (self.advanced.qr_code != defaults.qr_code).then_some(self.advanced.qr_code),
            ca_name_constraints: (self.advanced.ca_name_constraints
                != defaults.ca_name_constraints)
                .then_some(self.advanced.ca_name_constraints),
        };
        let out = OutConfig {
            cert,
            key,
            access_log: match &self.access_log {
                AccessLog::Stdout => "-".into(),
                AccessLog::Off => "off".into(),
                AccessLog::File(p) => path(p),
            },
            error_log: match &self.error_log {
                ErrorLog::Stderr => "-".into(),
                ErrorLog::File(p) => path(p),
            },
            log_format: self.log_format.to_string(),
            max_body: self.max_body.map(format_size),
            redirect: self.redirect,
            pid_file: self.pid_file.as_deref().map(path),
            proxy: self
                .listeners
                .iter()
                .map(|l| {
                    let (cert, key) = cert_key_out(l.cert_key.as_ref());
                    let m = &l.mapping;
                    OutProxy {
                        listen: match (m.bind, m.redirect_from) {
                            (None, None) => toml::Value::Integer(m.port.into()),
                            _ => toml::Value::String(m.listen_display()),
                        },
                        upstream: match &m.upstream {
                            Upstream::Tcp { host, port }
                                if host == mapping::DEFAULT_UPSTREAM_HOST =>
                            {
                                toml::Value::Integer((*port).into())
                            }
                            Upstream::Tcp { host, port } if host.contains(':') => {
                                toml::Value::String(format!("[{host}]:{port}"))
                            }
                            Upstream::Tcp { host, port } => {
                                toml::Value::String(format!("{host}:{port}"))
                            }
                            Upstream::Unix(p) => toml::Value::String(path(p)),
                        },
                        cert,
                        key,
                        max_body: l.max_body.map(format_size),
                    }
                })
                .collect(),
            advanced: (advanced != OutAdvanced::default()).then_some(advanced),
        };
        toml::to_string(&out).expect("config serializes to TOML")
    }
}

fn cert_key_out(ck: Option<&CertKey>) -> (Option<String>, Option<String>) {
    match ck {
        Some(ck) => (
            Some(ck.cert.display().to_string()),
            Some(ck.key.display().to_string()),
        ),
        None => (None, None),
    }
}

#[derive(Serialize)]
struct OutConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    cert: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    access_log: String,
    error_log: String,
    log_format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_body: Option<String>,
    redirect: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid_file: Option<String>,
    proxy: Vec<OutProxy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    advanced: Option<OutAdvanced>,
}

#[derive(Serialize)]
struct OutProxy {
    listen: toml::Value,
    upstream: toml::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cert: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_body: Option<String>,
}

#[derive(Serialize, Default, PartialEq)]
struct OutAdvanced {
    #[serde(skip_serializing_if = "Option::is_none")]
    connect_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    handshake_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shutdown_grace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qr_code: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ca_name_constraints: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    cert: Option<String>,
    key: Option<String>,
    access_log: Option<String>,
    error_log: Option<String>,
    log_format: Option<LogFormat>,
    max_body: Option<Spanned<toml::Value>>,
    redirect: Option<bool>,
    pid_file: Option<String>,
    #[serde(default)]
    proxy: Vec<Spanned<FileProxy>>,
    #[serde(default)]
    advanced: FileAdvanced,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileProxy {
    listen: Spanned<toml::Value>,
    upstream: Spanned<toml::Value>,
    cert: Option<String>,
    key: Option<String>,
    max_body: Option<Spanned<toml::Value>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileAdvanced {
    connect_timeout: Option<Spanned<toml::Value>>,
    handshake_timeout: Option<Spanned<toml::Value>>,
    idle_timeout: Option<Spanned<toml::Value>>,
    shutdown_grace: Option<Spanned<toml::Value>>,
    qr_code: Option<bool>,
    ca_name_constraints: Option<bool>,
}

/// Reads and validates a config file. Relative paths inside it are resolved
/// against the file's directory.
fn load_file(path: &Path) -> Result<Config, Vec<String>> {
    let path = absolute(path);
    let name = path.display().to_string();
    let text = std::fs::read_to_string(&path).map_err(|e| vec![format!("{name}: {e}")])?;
    let file: FileConfig =
        toml::from_str(&text).map_err(|e| vec![format!("{name}: {}", e.to_string().trim_end())])?;
    let base = path.parent().unwrap_or(Path::new("/")).to_path_buf();

    let mut errors = Vec::new();
    let at = |span: std::ops::Range<usize>| format!("{name}:{}", line_of(&text, span.start));
    let mut config = Config {
        config_path: Some(path.clone()),
        ..Config::default()
    };

    config.cert_key = match cert_key(file.cert, file.key, &base) {
        Ok(ck) => ck,
        Err(e) => {
            errors.push(format!("{name}: {e}"));
            None
        }
    };
    if let Some(s) = file.access_log {
        config.access_log = AccessLog::parse(&s, &base);
    }
    if let Some(s) = file.error_log {
        match ErrorLog::parse(&s, &base) {
            Ok(log) => config.error_log = log,
            Err(e) => errors.push(format!("{name}: error_log: {e}")),
        }
    }
    if let Some(format) = file.log_format {
        config.log_format = format;
    }
    if let Some(v) = file.max_body {
        match value_str(v.get_ref()).and_then(|s| parse_size(&s)) {
            Ok(n) => config.max_body = Some(n),
            Err(e) => errors.push(format!("{}: max_body: {e}", at(v.span()))),
        }
    }
    if let Some(redirect) = file.redirect {
        config.redirect = redirect;
    }
    if let Some(p) = file.pid_file {
        config.pid_file = Some(absolute(&base.join(p)));
    }

    for entry in file.proxy {
        let span = entry.span();
        let proxy = entry.into_inner();
        let listen = value_str(proxy.listen.get_ref()).and_then(|s| mapping::parse_listen(&s));
        let upstream =
            value_str(proxy.upstream.get_ref()).and_then(|s| mapping::parse_upstream(&s));
        let mapping::Listen {
            bind,
            port,
            redirect_from,
        } = match listen {
            Ok(l) => l,
            Err(e) => {
                errors.push(format!("{}: listen: {e}", at(proxy.listen.span())));
                continue;
            }
        };
        let mut upstream = match upstream {
            Ok(u) => u,
            Err(e) => {
                errors.push(format!("{}: upstream: {e}", at(proxy.upstream.span())));
                continue;
            }
        };
        if let Upstream::Unix(p) = &mut upstream {
            *p = absolute(&base.join(&p));
        }
        let cert_key = cert_key(proxy.cert, proxy.key, &base)
            .map_err(|e| errors.push(format!("{}: {e}", at(span.clone()))))
            .ok()
            .flatten();
        let max_body = match proxy.max_body {
            Some(v) => match value_str(v.get_ref()).and_then(|s| parse_size(&s)) {
                Ok(n) => Some(n),
                Err(e) => {
                    errors.push(format!("{}: max_body: {e}", at(v.span())));
                    None
                }
            },
            None => None,
        };
        config.listeners.push(Listener {
            mapping: Mapping {
                bind,
                port,
                redirect_from,
                upstream,
            },
            cert_key,
            max_body,
        });
    }

    let adv = file.advanced;
    let defaults = Advanced::default();
    let mut duration = |v: Option<Spanned<toml::Value>>, key: &str, default: Duration| match v {
        None => default,
        Some(v) => match value_str(v.get_ref()).and_then(|s| parse_duration(&s)) {
            Ok(d) => d,
            Err(e) => {
                errors.push(format!("{}: {key}: {e}", at(v.span())));
                default
            }
        },
    };
    config.advanced = Advanced {
        connect_timeout: duration(
            adv.connect_timeout,
            "connect_timeout",
            defaults.connect_timeout,
        ),
        handshake_timeout: duration(
            adv.handshake_timeout,
            "handshake_timeout",
            defaults.handshake_timeout,
        ),
        idle_timeout: duration(adv.idle_timeout, "idle_timeout", defaults.idle_timeout),
        shutdown_grace: duration(
            adv.shutdown_grace,
            "shutdown_grace",
            defaults.shutdown_grace,
        ),
        qr_code: adv.qr_code.unwrap_or(defaults.qr_code),
        ca_name_constraints: adv
            .ca_name_constraints
            .unwrap_or(defaults.ca_name_constraints),
    };

    if errors.is_empty() {
        Ok(config)
    } else {
        Err(errors)
    }
}

fn cert_key(
    cert: Option<String>,
    key: Option<String>,
    base: &Path,
) -> Result<Option<CertKey>, String> {
    match (cert, key) {
        (Some(cert), Some(key)) => Ok(Some(CertKey {
            cert: absolute(&base.join(cert)),
            key: absolute(&base.join(key)),
        })),
        (None, None) => Ok(None),
        (Some(_), None) => Err("`cert` is set but `key` is missing".into()),
        (None, Some(_)) => Err("`key` is set but `cert` is missing".into()),
    }
}

/// Accepts a TOML integer or string and returns it as a string.
fn value_str(v: &toml::Value) -> Result<String, String> {
    match v {
        toml::Value::Integer(n) => Ok(n.to_string()),
        toml::Value::String(s) => Ok(s.clone()),
        other => Err(format!(
            "expected a number or a string, got {}",
            other.type_str()
        )),
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// Makes a path absolute without touching the filesystem.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Parses a byte size: `1048576`, `512K`, `10M`, `1G` (binary multiples;
/// a trailing `B` and lowercase are accepted).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let t = t.strip_suffix(['b', 'B']).unwrap_or(t);
    let (digits, mult) = match t.chars().last() {
        Some('k' | 'K') => (&t[..t.len() - 1], 1u64 << 10),
        Some('m' | 'M') => (&t[..t.len() - 1], 1 << 20),
        Some('g' | 'G') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    digits
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("`{s}` is not a size; use e.g. 512K, 10M or 1G"))
}

/// Parses a duration: `250ms`, `5s`, `2m`, `1h`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    let err = || format!("`{s}` is not a duration; use e.g. 500ms, 5s, 2m or 1h");
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let n: u64 = t[..split].parse().map_err(|_| err())?;
    let d = match t[split..].trim() {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => return Err(err()),
    };
    Ok(d)
}

/// Formats a duration the way [`parse_duration`] reads it.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if !ms.is_multiple_of(1000) {
        format!("{ms}ms")
    } else if ms.is_multiple_of(3_600_000) && ms > 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) && ms > 0 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}s", ms / 1000)
    }
}

/// Formats a byte size the way [`parse_size`] reads it.
pub fn format_size(n: u64) -> String {
    for (suffix, mult) in [("G", 1u64 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if n.is_multiple_of(mult) {
            return format!("{}{suffix}", n / mult);
        }
    }
    n.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::Write;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let dir = std::env::temp_dir().join(format!(
                "ferry-test-{}-{}",
                std::process::id(),
                crate::proxy::random_id(12)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::File::create(&path)
                .unwrap()
                .write_all(contents.as_bytes())
                .unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn load(args: &[&str]) -> Result<Config, Vec<String>> {
        let argv = std::iter::once("ferry").chain(args.iter().copied());
        Config::load(&Cli::try_parse_from(argv).unwrap())
    }

    fn load_toml(toml: &str, extra: &[&str]) -> (TempDir, Result<Config, Vec<String>>) {
        let dir = TempDir::new();
        let path = dir.file("ferry.toml", toml);
        let mut args = vec!["-c", path.to_str().unwrap()];
        args.extend_from_slice(extra);
        let result = load(&args);
        (dir, result)
    }

    #[test]
    fn cli_only() {
        let c = load(&["6005:6006"]).unwrap();
        assert_eq!(c.listeners.len(), 1);
        assert_eq!(c.cert_key, None);
        assert!(c.redirect);
        assert_eq!(c.access_log, AccessLog::Stdout);
        assert_eq!(c.log_format, LogFormat::Combined);
    }

    #[test]
    fn cli_paths_are_absolute() {
        let c = load(&["cert.pem", "key.pem", "6005:./app.sock"]).unwrap();
        let ck = c.cert_key.unwrap();
        assert!(ck.cert.is_absolute() && ck.key.is_absolute());
        match &c.listeners[0].mapping.upstream {
            Upstream::Unix(p) => assert!(p.is_absolute() && p.ends_with("app.sock")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cli_paths_resolve_against_given_dir() {
        let cli = Cli::try_parse_from(["ferry", "c.pem", "k.pem", "6005:./a.sock"]).unwrap();
        let c = Config::load_in(&cli, Path::new("/srv/app")).unwrap();
        assert_eq!(c.cert_key.unwrap().cert, PathBuf::from("/srv/app/c.pem"));
        assert_eq!(
            c.listeners[0].mapping.upstream,
            Upstream::Unix("/srv/app/a.sock".into())
        );
    }

    #[test]
    fn full_file() {
        let (dir, c) = load_toml(
            r#"
cert = "certs/cert.pem"
key = "certs/key.pem"
max_body = "10M"
access_log = "logs/access.log"
error_log = "-"
log_format = "json"
redirect = false
pid_file = "run/ferry.pid"

[[proxy]]
listen = 6005
upstream = 6006

[[proxy]]
listen = "127.0.0.1:7001"
upstream = "10.0.0.5:6006"
max_body = 1024

[[proxy]]
listen = 8443
upstream = "./app.sock"
cert = "other.pem"
key = "other.key"

[advanced]
connect_timeout = "2s"
idle_timeout = 30
"#,
            &[],
        );
        let c = c.unwrap();
        let base = &dir.0;
        assert_eq!(
            c.cert_key.as_ref().unwrap().cert,
            base.join("certs/cert.pem")
        );
        assert_eq!(c.max_body, Some(10 << 20));
        assert_eq!(c.access_log, AccessLog::File(base.join("logs/access.log")));
        assert_eq!(c.error_log, ErrorLog::Stderr);
        assert_eq!(c.log_format, LogFormat::Json);
        assert!(!c.redirect);
        assert_eq!(c.pid_file, Some(base.join("run/ferry.pid")));
        assert_eq!(c.listeners.len(), 3);
        assert_eq!(c.listeners[0].mapping.port, 6005);
        assert_eq!(
            c.listeners[1].mapping.upstream,
            Upstream::Tcp {
                host: "10.0.0.5".into(),
                port: 6006
            }
        );
        assert_eq!(c.listeners[1].max_body(&c), Some(1024));
        assert_eq!(c.listeners[0].max_body(&c), Some(10 << 20));
        assert_eq!(
            c.listeners[2].mapping.upstream,
            Upstream::Unix(base.join("app.sock"))
        );
        assert_eq!(
            c.listeners[2].cert_key(&c).unwrap().cert,
            base.join("other.pem")
        );
        assert_eq!(c.advanced.connect_timeout, Duration::from_secs(2));
        assert_eq!(c.advanced.idle_timeout, Duration::from_secs(30));
        assert_eq!(c.advanced.handshake_timeout, Duration::from_secs(15));
        assert!(c.advanced.qr_code && c.advanced.ca_name_constraints);
    }

    #[test]
    fn errors_have_line_numbers_and_are_all_reported() {
        let (_dir, r) = load_toml(
            "[[proxy]]\nlisten = 6005\nupstream = 99999\n\n[[proxy]]\nlisten = \"localhost:1\"\nupstream = 2\n",
            &[],
        );
        let errors = r.unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0].ends_with(":3: upstream: port `99999` is out of range (1-65535)"),
            "{}",
            errors[0]
        );
        assert!(
            errors[1].contains(":6: listen: listen address `localhost` must be an IP address"),
            "{}",
            errors[1]
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let (_dir, r) = load_toml("[[proxy]]\nlisten = 1\nupstream = 2\nupstraem = 3\n", &[]);
        let e = r.unwrap_err().join("\n");
        assert!(e.contains("unknown field `upstraem`"), "{e}");
        assert!(e.contains("line 4"), "{e}");
    }

    #[test]
    fn cli_pid_file_overrides_file_and_is_relative_to_cwd() {
        let (_dir, c) = load_toml(
            "pid_file = \"a.pid\"\n[[proxy]]\nlisten = 1\nupstream = 2\n",
            &["--pid-file", "b.pid"],
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(c.unwrap().pid_file, Some(cwd.join("b.pid")));
    }

    #[test]
    fn cli_overrides_file() {
        let (_dir, c) = load_toml(
            "max_body = \"1M\"\nlog_format = \"json\"\n[[proxy]]\nlisten = 6005\nupstream = 6006\n",
            &[
                "--max-body",
                "2M",
                "--log-format",
                "common",
                "--no-redirect",
                "7001:7002",
            ],
        );
        let c = c.unwrap();
        assert_eq!(c.max_body, Some(2 << 20));
        assert_eq!(c.log_format, LogFormat::Common);
        assert!(!c.redirect);
        assert_eq!(c.listeners.len(), 2);
    }

    #[test]
    fn conflicts_between_file_and_cli() {
        let (_dir, r) = load_toml(
            "[[proxy]]\nlisten = 6005\nupstream = 6006\n",
            &["6005:7000"],
        );
        assert_eq!(
            r.unwrap_err(),
            vec!["`6005` and `6005` both listen on port 6005"]
        );
    }

    #[test]
    fn cert_without_key() {
        let (_dir, r) = load_toml(
            "cert = \"c.pem\"\n[[proxy]]\nlisten = 1\nupstream = 2\n",
            &[],
        );
        assert!(r.unwrap_err()[0].ends_with("`cert` is set but `key` is missing"));
    }

    #[test]
    fn empty_file_needs_a_mapping() {
        let (_dir, r) = load_toml("", &[]);
        assert!(r.unwrap_err()[0].contains("add a [[proxy]] section"));
        let (_dir, r) = load_toml("", &["6005:6006"]);
        assert!(r.is_ok());
    }

    #[test]
    fn missing_file() {
        let e = load(&["-c", "/nonexistent/ferry.toml"]).unwrap_err();
        assert!(e[0].starts_with("/nonexistent/ferry.toml: "), "{e:?}");
    }

    #[test]
    fn print_config_round_trips() {
        let (dir, original) = load_toml(
            r#"
cert = "c.pem"
key = "k.pem"
max_body = "10M"
access_log = "access.log"
log_format = "json"
redirect = false
pid_file = "ferry.pid"

[[proxy]]
listen = "[::1]:6005"
upstream = "[fe80::1]:6006"
max_body = 1000

[[proxy]]
listen = 7001
upstream = "./app.sock"
cert = "o.pem"
key = "o.key"

[advanced]
idle_timeout = "2m"
qr_code = false
ca_name_constraints = false
"#,
            &["127.0.0.1:8000:9000", "8001:app:80", "8080/8443:9001"],
        );
        let original = original.unwrap();
        let printed = original.to_toml();
        let path = dir.file("printed.toml", &printed);
        let reloaded = load(&["-c", path.to_str().unwrap()]).unwrap();
        assert_eq!(
            Config {
                config_path: None,
                ..reloaded
            },
            Config {
                config_path: None,
                ..original
            },
            "{printed}"
        );
        assert!(printed.contains("idle_timeout = \"2m\""), "{printed}");
        assert!(printed.contains("qr_code = false"), "{printed}");
        assert!(printed.contains("ca_name_constraints = false"), "{printed}");
        assert!(!printed.contains("connect_timeout"), "{printed}");
    }

    #[test]
    fn example_file_is_valid() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/ferry.example.toml");
        let c = load(&["-c", path]).unwrap();
        assert_eq!(c.listeners.len(), 1);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("512K").unwrap(), 512 << 10);
        assert_eq!(parse_size("10mb").unwrap(), 10 << 20);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("0").is_err());
        assert_eq!(format_size(10 << 20), "10M");
        assert_eq!(format_size(1000), "1000");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert!(parse_duration("5 days").is_err());
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1500ms");
    }
}
