# ferry

**A very simple HTTPS reverse proxy.** Point it at an app that speaks plain
HTTP and get HTTPS in front of it, with no configuration:

```sh
ferry 6005:6006
```

```
ferry 0.1.0
  https://localhost:6005  →  http://localhost:6006
  https://192.168.1.20:6005
  cert: ferry local CA (~/Library/Application Support/ferry/ca.pem)
  CA fingerprint (SHA-256):
    03:0B:A0:99:DC:39:A1:83:8F:C7:78:3B:B2:1A:DB:BB
    62:15:FB:C0:95:2B:0B:AF:8F:B7:F8:67:E0:EC:72:DA
  browsers warn until you trust the CA; see http://localhost:6005/.ferry/
  on a phone, scan this first:          Trust ferry's CA, once per phone:
  (QR code)                             iPhone
                                         • Open the page in Safari and ...
  http://192.168.1.20:6005/.ferry/
  then open the app on :6005:
  (QR code)
  https://192.168.1.20:6005/
```

**HTTPS for development, with a path to production.** ferry gives your dev
server HTTPS in one command: no nginx or Caddy setup, no certificates to make.
When the project is ready to ship, ferry will write you an equivalent nginx or
Caddy config (coming in v0.3), so what worked in development carries over.

ferry runs on macOS and Linux. It's a single binary with no runtime
dependencies.

## Why ferry

ferry does one job, and its defaults are chosen so you rarely need to
configure anything:

- **No certificate needed.** Without one, ferry issues one from its own local
  certificate authority, covering `localhost`, your hostname and your LAN
  addresses. Trust the CA once and it stays trusted, even as your addresses
  change.
- **Test on your phone.** Scan the QR code in the banner and ferry walks you
  through trusting its CA on iOS or Android, so service workers and PWAs
  work on a real device.
- **HTTP/2 and HTTP/1.1** to clients, negotiated automatically.
- **WebSockets just work**, so dev-server hot reload (Vite, Next.js, …) works
  through ferry.
- **Runs your dev server too.** `ferry 6005:5173 -- npm run dev` starts it,
  holds requests until it's listening, and exits when it does.
- **Plain HTTP is redirected to HTTPS**, both on the same port
  (`http://localhost:6005` → `https://localhost:6005`) and from port 80 to
  443.
- **IPv4 and IPv6** on one socket. Upstream `localhost` works whether your
  app listens on `127.0.0.1` or `::1`.
- **Unix socket upstreams**: `ferry 443:/run/app.sock`.
- **Live reload.** Edit the config file or renew a certificate and ferry
  applies the change without dropping connections. A broken edit is rejected
  and the running config keeps going.
- **Helpful errors.** When your app isn't running, you get a page that says
  *"nothing is listening on localhost:6006 — is your app running?"*, not a
  bare 502.
- Access logs (common, combined or JSON), request IDs, `X-Forwarded-*`
  headers, body size limits, graceful shutdown, a config checker, and
  background mode with a pid file.

## Install

From source, with Rust 1.88 or newer:

```sh
git clone https://github.com/octoth0rpe/ferry.git
cd ferry
cargo install --path .
```

This installs to `~/.cargo/bin/ferry`, which rustup puts on your `PATH`. To
install somewhere else, pass `--root`: `cargo install --path . --root
/usr/local` installs to `/usr/local/bin/ferry`.

Or run `cargo build --release`; the binary is at `target/release/ferry`.

## Quick start

```sh
# HTTPS on 6005 for an app on 6006, with a certificate from ferry's local CA
ferry 6005:6006

# Your own certificate, several apps at once
ferry ./fullchain.pem ./privkey.pem 443:3000 8443:3001

# An app listening on a Unix socket
ferry 443:/run/app.sock

# Everything in a config file
ferry -c ferry.toml

# Start your dev server along with ferry
ferry 6005:5173 -- npm run dev
```

### Running your app with ferry

Everything after `--` is a command ferry runs alongside itself, usually your
dev server:

```sh
ferry 6005:5173 -- npm run dev
```

- The command shares the terminal: you see its output, and it can read your
  keystrokes (Vite's shortcuts work).
- Requests that arrive before the app is listening wait for it, for up to 30
  seconds, instead of getting an error page.
- ferry's banner, with its URLs and QR code, appears once the app is
  listening, below the app's own startup output, even if the dev server
  clears the screen.
- Ctrl-C stops both. ferry passes SIGTERM on, and exits when the command
  does, with its exit code.

It's for running in the foreground: `-d` with a command is an error.

## Mappings

Each mapping is `LISTEN:UPSTREAM`. The listen side is the HTTPS port, with
an optional address to bind. The upstream is where requests are sent, over
plain HTTP.

| Mapping                   | Listens on        | Forwards to                |
|---------------------------|-------------------|----------------------------|
| `6005:6006`               | all interfaces    | `http://localhost:6006`    |
| `127.0.0.1:6005:6006`     | `127.0.0.1` only  | `http://localhost:6006`    |
| `6005:10.0.0.5:6006`      | all interfaces    | `http://10.0.0.5:6006`     |
| `6005:app.internal:8080`  | all interfaces    | `http://app.internal:8080` |
| `6005:http://app:8080`    | all interfaces    | `http://app:8080`          |
| `6005:/run/app.sock`      | all interfaces    | Unix socket                |
| `6005:./app.sock`         | all interfaces    | Unix socket, relative path |
| `[::1]:6005:[::1]:6006`   | `::1` only        | `http://[::1]:6006`        |

IPv6 addresses go in brackets. A Unix socket path must start with `/`, `./`
or `../`.

If you give a certificate and key, they come before the mappings. ferry
works out which arguments are files and which are mappings, so the order
doesn't matter, but it needs both files or neither.

## Certificates

### Your own

Pass a PEM certificate and key, either on the command line or as `cert` and
`key` in the config file. Use the **full chain**: your certificate followed by
any intermediates. With Let's Encrypt that's `fullchain.pem`, not `cert.pem`.
Without the full chain, browsers often cope but `curl` and other clients
reject the connection. ferry warns if a CA-issued certificate has no
intermediates.

The key must be unencrypted (PKCS#8, PKCS#1/RSA or SEC1/EC). ferry checks the
key matches the certificate before it starts.

When the certificate or key files change, ferry loads them automatically, so
renewals by certbot or similar tools need no restart. Each listener in a
config file can have its own `cert` and `key`.

### The local CA

With no certificate configured, ferry creates its own certificate authority
the first time and keeps it, with its key, at:

- macOS: `~/Library/Application Support/ferry/ca.pem`
- Linux: `~/.local/share/ferry/ca.pem` (or `$XDG_DATA_HOME/ferry/`)

The CA is valid for 10 years. It has *name constraints*: it can only vouch
for `localhost`, names ending in `.local`, and loopback and private
addresses. A trusted CA can otherwise vouch for any site, so this limits the
damage if its key ever leaks (Android ignores name constraints, so there the
protection doesn't apply). The certificate covers your machine as
`hostname.local` but not as the bare hostname: use `.local` or an address
from other machines. If your machine has an address the CA can't cover, such
as a public IP, ferry leaves it out of the certificate and says so. If you need a CA without constraints, set
`ca_name_constraints = false` under `[advanced]` and delete `ca.pem` and
`ca-key.pem`; the setting only applies when the CA is created.

On every start, ferry issues a new server
certificate from it (`server.pem` in the same directory) covering
`localhost`, `127.0.0.1`, `::1`, your hostname (and `hostname.local`), and the
machine's LAN IPv4 addresses. While it runs, it issues another when those
change, such as when your laptop joins another network. Nothing trusts the
server certificate directly, so replacing it costs nothing.

Browsers warn until you trust the CA. Open `http://localhost:PORT/.ferry/`,
over plain HTTP, on any port that uses it. The page has the commands for
this machine, filled in with the real paths:

```sh
# macOS
security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db \
  ~/"Library/Application Support/ferry/ca.pem"

# Debian/Ubuntu
sudo cp ~/.local/share/ferry/ca.pem /usr/local/share/ca-certificates/ferry.crt
sudo update-ca-certificates

# Fedora, Arch, openSUSE
sudo trust anchor --store ~/.local/share/ferry/ca.pem
```

Firefox, and Chrome on Linux, keep their own certificate stores (NSS). The
page lists a `certutil` command for each one ferry finds. On macOS you can
instead set `security.enterprise_roots.enabled` to `true` in Firefox's
`about:config`, so it uses the keychain.

**Phones.** When stderr is a terminal, the banner shows a QR code for
`http://<your LAN address>:PORT/.ferry/`, with the steps beside it, and below
that a QR code for each app. Scan the first and follow the steps for your
phone:

- **iPhone and iPad:** open the page in Safari (only Safari can install
  profiles) and download the certificate. Install the profile from Settings →
  General → VPN & Device Management within 8 minutes (iOS discards it after
  that), then turn on full trust under Settings → General → About →
  Certificate Trust Settings.
- **Android:** download the certificate, then install it from Settings →
  Security → Encryption & credentials → Install a certificate → CA
  certificate. Android then shows "Network may be monitored"; that's expected.

The page comes over plain HTTP, so check the fingerprint on it matches the one
in ferry's banner. Over HTTPS, `/.ferry/` goes to your app like any other
path. To leave the QR codes out of the banner, set `qr_code = false` under
`[advanced]`.

Keep `ca-key.pem` private all the same: anyone with it could impersonate your
local sites to devices that trust your CA.

**Upgrading from the self-signed certificate.** Earlier versions of ferry
used a self-signed certificate (`self-signed.pem`). It's no longer used:
trust the CA once instead, and remove the old certificate from your trust
store if you added it.

## Config file

Everything that can be set on the command line can also go in a TOML file,
plus a few rarely needed settings. The exceptions are `-d`, `-q` and `-t`,
which are about how you run ferry rather than what it serves. Every key is
optional except at least one `[[proxy]]`. Every path in the file (certificates,
logs, the pid file and Unix sockets) can be relative, and relative paths are
resolved against the config file's directory.

```toml
cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
key  = "/etc/letsencrypt/live/example.com/privkey.pem"

access_log = "/var/log/ferry/access.log"   # "-" = stdout (default), "off"
error_log  = "/var/log/ferry/error.log"    # "-" = stderr (default)
log_format = "combined"                    # "common", "combined", "json"
max_body   = "10M"                         # default: unlimited
redirect   = true                          # default: true
pid_file   = "/run/ferry/ferry.pid"        # default: none

[[proxy]]
listen   = 443
upstream = 3000

[[proxy]]
listen   = "127.0.0.1:8443"
upstream = "/run/admin.sock"
cert     = "admin-cert.pem"                # per-listener certificate
key      = "admin-key.pem"
max_body = "100M"                          # per-listener limit
```

See [`ferry.example.toml`](ferry.example.toml) for every option with comments.

Command-line flags override the file, and mappings on the command line are
added to the file's `[[proxy]]` entries. Unknown keys are errors, so typos
don't go unnoticed. Errors give the file and line number.

### Checking and generating configs

```sh
ferry -t -c ferry.toml        # check everything, report all problems, exit 0 or 1
ferry --print-config 443:3000 --max-body 10M > ferry.toml   # flags → config file
```

`-t` loads the certificates and checks they match their keys and haven't
expired; it warns if they expire within 14 days. It also warns about busy
ports and missing Unix sockets. It has no side effects: it never generates a
certificate or creates log files.

## Reloading

With `-c`, ferry watches the config file and every certificate and key it
uses, checking them once a second. When one changes, ferry checks the new configuration the same way `-t`
does:

- **If it's valid**, ferry applies it in place. New listeners start, removed
  ones stop, and changed upstreams and certificates take effect for new
  connections. Requests in progress finish undisturbed.
- **If it isn't**, ferry logs the problems and keeps running the current
  configuration.

Without `-c`, the certificate files given on the command line are watched the
same way.

## Signals

| Signal              | Effect                                                                 |
|---------------------|------------------------------------------------------------------------|
| `SIGTERM`, `SIGINT` | Stop accepting connections; let in-flight requests finish (up to 10s)  |
| second `SIGINT`     | Exit immediately                                                       |
| `SIGHUP`            | Reload the configuration and reopen log files                          |
| `SIGUSR1`           | Reopen log files (for logrotate)                                       |

## Logging

The **access log** has one line per request. It's written when the response
has been fully sent, so byte counts and durations are accurate. It goes to
stdout by default; `-q` turns that off, and `--access-log FILE` writes to a
file instead.

- `combined` (default): Apache's combined layout, plus duration and request ID:
  `127.0.0.1 - - [2026-10-16T03:00:00.123Z] "GET / HTTP/2.0" 200 1234 "-" "curl/8.7" 3.1ms Xk3k9QpL2mN8vR4t`
- `common`: Apache's common log layout.
- `json`: one object per line with every field (time, client, method, path,
  protocol, host, status, bytes, duration, request ID, upstream, referer,
  user agent).

The **error log** (startup, warnings, upstream failures, reloads) goes to
stderr, or to `--error-log FILE`. Set `FERRY_LOG` to a level (`error`,
`warn`, `info`, `debug` or `trace`) to change how much it says; the default
is `info`.

Every timestamp ferry writes, in both logs and all formats, is ISO 8601 in
UTC with milliseconds: `2026-10-16T03:00:00.123Z`. (In `common` and
`combined` this replaces Apache's `[16/Oct/2026:03:00:00 +0000]`, so tools
that parse those formats need their date format set accordingly.)

## What ferry does to requests

- Forwards the method, path, query and headers unchanged, and keeps `Host`
  as the client sent it.
- Removes hop-by-hop headers (`Connection` and the headers it names,
  `Keep-Alive`, `TE`, `Transfer-Encoding`, `Upgrade`, `Proxy-*`).
- Adds `X-Forwarded-For` (appended to any existing value),
  `X-Forwarded-Proto: https`, `X-Forwarded-Host` and `X-Forwarded-Port`.
- Adds an `X-Request-Id` if the client didn't send one, passes it to the
  upstream, returns it in the response, and logs it.
- Streams request and response bodies without buffering them.
- Talks HTTP/1.1 to upstreams, with pooled keep-alive connections.
- Passes `Upgrade` requests (WebSockets) through, in both directions. Clients
  open WebSockets over HTTP/1.1; browsers do this automatically.
- `--max-body` rejects larger requests with `413`, for both `Content-Length`
  and chunked bodies.
- Answers `502` with an explanation when the upstream can't be reached, and
  `504` when connecting times out. Browsers get a small HTML page; other
  clients get plain text.

## Running in the background

```sh
ferry -d -c /etc/ferry/ferry.toml
```

with `pid_file`, `access_log` and `error_log` set in the config file, or all
on the command line:

```sh
ferry -d --pid-file ferry.pid --access-log access.log --error-log error.log 443:3000
```

Relative paths on the command line are resolved against the directory you
start ferry in.

`-d` checks the configuration and binds the ports before going into the
background. Startup errors still appear in your terminal and the exit status
reflects them. ferry warns if logs would go to the terminal, since they'd be
discarded. The pid file (`pid_file`, or `--pid-file`) also works without `-d`.
ferry refuses to start if the pid file names a running process, and removes
the file on exit. `-d` is deliberately command-line only, so a config file
used under systemd or launchd can't make ferry fork by accident.

### systemd

Under systemd, don't use `-d`; systemd manages the process itself.

```ini
# /etc/systemd/system/ferry.service
[Unit]
Description=ferry HTTPS proxy
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/ferry -c /etc/ferry/ferry.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
User=ferry
# Bind ports 80 and 443 without running as root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
# Somewhere to keep ferry's local CA, if you use it.
StateDirectory=ferry
Environment=XDG_DATA_HOME=/var/lib

[Install]
WantedBy=multi-user.target
```

### launchd (macOS)

```xml
<!-- ~/Library/LaunchAgents/local.ferry.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>local.ferry</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/ferry</string>
    <string>-c</string><string>/Users/you/.config/ferry/ferry.toml</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/Users/you/Library/Logs/ferry.log</string>
</dict>
</plist>
```

Load it with `launchctl load ~/Library/LaunchAgents/local.ferry.plist`.

## Defaults

| Setting                          | Default                                | Change it with                                |
|----------------------------------|----------------------------------------|-----------------------------------------------|
| Bind address                     | all interfaces, IPv4 + IPv6            | `ADDR:PORT:UPSTREAM`, `listen = "ADDR:PORT"`  |
| Upstream host                    | `localhost` (`::1` and `127.0.0.1`)    | `PORT:HOST:PORT`                              |
| Certificate                      | issued from ferry's local CA           | `CERT KEY`, `cert`/`key`                      |
| HTTP → HTTPS redirect            | on                                     | `--no-redirect`, `redirect = false`           |
| Body size limit                  | none                                   | `--max-body`, `max_body`                      |
| Access log                       | stdout, `combined`                     | `--access-log`, `--log-format`, `-q`          |
| Error log                        | stderr                                 | `--error-log`                                 |
| Upstream connect timeout         | 5s                                     | `[advanced] connect_timeout`                  |
| TLS handshake + request headers  | 15s                                    | `[advanced] handshake_timeout`                |
| Idle connections                 | 90s                                    | `[advanced] idle_timeout`                     |
| Shutdown grace period            | 10s                                    | `[advanced] shutdown_grace`                   |
| QR codes for phones in the banner | on, when stderr is a terminal         | `[advanced] qr_code`                          |
| Local CA name constraints        | on (local names, private addresses)    | `[advanced] ca_name_constraints`              |

## Shell completions and man page

```sh
ferry --completions zsh  > ~/.zfunc/_ferry          # or bash, fish, elvish
ferry --completions bash > /etc/bash_completion.d/ferry
sudo install -m 644 doc/ferry.1 /usr/local/share/man/man1/   # then: man ferry
```

## Development

```sh
cargo build
cargo test            # unit tests plus end-to-end tests that run the binary
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo deny check      # security advisories, banned crates, licenses (deny.toml)
```

A pre-commit hook runs all of these. Enable it once per clone:

```sh
cargo install --locked cargo-deny
git config core.hooksPath .githooks
```

### Testing on a phone

`tools/pwa-test/` is a tiny installable web app for checking the local CA on
real devices: run it with `python3 -m http.server` behind ferry. Its README
has the steps and what to check.

### Updating dependencies

```sh
cargo update                    # every crate, within the versions Cargo.toml allows
cargo update -p rustls          # just one crate
cargo test && cargo deny check
```

Commit the updated `Cargo.lock`. When `cargo deny check advisories` reports a
vulnerable crate, it names the fixed version. If `cargo update -p <crate>`
doesn't reach that version, raise the requirement in `Cargo.toml`, or for a
transitive dependency, update whichever crate pulls it in (`cargo tree -i
<crate>` shows which).

The guiding principle: **do as much as possible by default.** Running ferry
with no configuration should cover 99% of uses. Prefer a working default to a
new flag, and keep rarely changed settings in the config file, not on the
command line.

## License

MIT
