//! Config file loading and the resolved runtime configuration.
//!
//! The effective configuration is built from an optional TOML file plus
//! command-line arguments. CLI flags override file values; CLI mappings are
//! added to the file's `[[proxy]]` entries.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use toml::Spanned;

use crate::cli::Cli;
use crate::mapping::{self, Mapping, Upstream};

/// A certificate chain and its private key, both PEM files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertKey {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Certificate for listeners without their own; `None` means self-signed.
    pub cert_key: Option<CertKey>,
    pub listeners: Vec<Listener>,
    pub access_log: AccessLog,
    pub error_log: ErrorLog,
    pub log_format: LogFormat,
    pub quiet: bool,
    /// Default request body limit in bytes; `None` means unlimited.
    pub max_body: Option<u64>,
    pub redirect: bool,
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
    /// Time allowed for a client's TLS handshake and request headers.
    pub handshake_timeout: Duration,
    /// How long idle keep-alive connections are kept open.
    pub idle_timeout: Duration,
    /// How long in-flight requests get to finish on shutdown.
    pub shutdown_grace: Duration,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced {
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(90),
            shutdown_grace: Duration::from_secs(10),
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
            advanced: Advanced::default(),
            config_path: None,
        }
    }
}

impl Listener {
    /// The certificate this listener serves; `None` means self-signed.
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
    pub fn load(cli: &Cli) -> Result<Config, Vec<String>> {
        let positionals = cli.positionals().map_err(|e| vec![e])?;
        let mut config = match &cli.config {
            Some(path) => load_file(path)?,
            None => Config::default(),
        };
        let mut errors = Vec::new();

        if let Some(ck) = positionals.cert_key {
            config.cert_key = Some(CertKey {
                cert: absolute(&ck.cert),
                key: absolute(&ck.key),
            });
        }
        for mut mapping in positionals.mappings {
            if let Upstream::Unix(path) = &mut mapping.upstream {
                *path = absolute(path);
            }
            config.listeners.push(Listener {
                mapping,
                cert_key: None,
                max_body: None,
            });
        }
        if let Some(s) = &cli.access_log {
            config.access_log = AccessLog::parse(s, Path::new("."));
        }
        if let Some(s) = &cli.error_log {
            match ErrorLog::parse(s, Path::new(".")) {
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
        config.quiet = cli.quiet;
        if cli.no_redirect {
            config.redirect = false;
        }

        for (i, a) in config.listeners.iter().enumerate() {
            if let Some(b) = config.listeners[..i]
                .iter()
                .find(|b| b.mapping.conflicts_with(&a.mapping))
            {
                errors.push(format!(
                    "`{}` and `{}` both listen on port {}",
                    b.mapping.listen_display(),
                    a.mapping.listen_display(),
                    a.mapping.port
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

    for entry in file.proxy {
        let span = entry.span();
        let proxy = entry.into_inner();
        let listen = value_str(proxy.listen.get_ref()).and_then(|s| mapping::parse_listen(&s));
        let upstream =
            value_str(proxy.upstream.get_ref()).and_then(|s| mapping::parse_upstream(&s));
        let (bind, port) = match listen {
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
    if ms % 1000 != 0 {
        format!("{ms}ms")
    } else if ms % 3_600_000 == 0 && ms > 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms % 60_000 == 0 && ms > 0 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}s", ms / 1000)
    }
}

/// Formats a byte size the way [`parse_size`] reads it.
pub fn format_size(n: u64) -> String {
    for (suffix, mult) in [("G", 1u64 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if n % mult == 0 {
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
                fastrand::u64(..)
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
