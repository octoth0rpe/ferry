# ferry

**A very simple HTTPS reverse proxy.** One command gives your dev server
HTTPS that your browser *and your phone* trust, without setting up nginx or
Caddy.

- **No certificates to make.** ferry runs its own local CA; trust it once
  per device.
- **Phones in a minute.** Scan one QR code to trust the CA, another to open
  your app. PWAs, service workers and camera APIs then just work.
- **Runs your dev server too**, and holds requests until it's listening.
- **Ready for a real server**: your own certificate, port 80 redirecting to
  443, and live reload when certbot renews.
- **One binary** for macOS and Linux, with nothing to configure.

```sh
ferry 5273:5173 -- npm run dev
```

```
ferry 0.2.0
  https://localhost:5273  →  http://localhost:5173
  https://192.168.1.20:5273
  cert: ferry local CA (~/Library/Application Support/ferry/ca.pem), SHA-256 03:0B:A0…EC:72:DA
  NOTE: Browsers will warn until you trust the CA; see http://192.168.1.20:52814/

  On a phone, scan this first:
  █████████████████████████████████    Trust ferry's CA, once per phone:
  ██ ▄▄▄▄▄ █▄▄▀▄█  █▄█▀▄▄█ ▄▄▄▄▄ ██    iPhone
  ██ █   █ ████▄  ▄█▀█  ▀█ █   █ ██     • Open the page in Safari and
  ██ █▄▄▄█ █▄ █▄▄▀▄▄▄▀▄ ██ █▄▄▄█ ██       download the certificate.
  ██▄▄▄▄▄▄▄███▀▄█▀█▀▄  █ █▄▄▄▄▄▄▄██     • Settings → General → VPN & Device
  ███▀ █   ▀█▀▄▀█▄▀   █▀▀▄ █▀ ▀▄▄██       Management → ferry local CA →
  ███  ▀▀█▀              ▄ ▀█▄ █▀██       Install (within 8 minutes).
  ██▀  ▀▀█▀  NOT A REAL   ▄▄▄ █ ▀██     • General → About → Certificate
  ████▄▀█▄▀   QR CODE    ▄▀█ ▀ ▀▄██       Trust Settings → ferry local CA.
  ██▄▄▄ ██▀              ▀ ▄▄█▄█▀██    Android
  ███▄█▀ ▄██ ▄   ██▀▀█ ███▀▄█▀▀█▀██     • Download the certificate.
  ██▄██▄██▄█▀█ ▀▄▀▄▀█▄ ▀▀▀ ▀▀▄▄▀▀██     • Settings → search "CA certificate"
  ██ ▄▄▄▄▄ █▄    ▄█ ▀▄▀ ▀▀▀▄██▄▄▀██       → Install anyway → ferry-ca.crt.
  ██ █   █ █▀▀█▄ ▄ █ ▀▀█▀▀▄█▄▀▀▀▄██
  ██ █▄▄▄█ ██▀█████▄▄ █▄▄█▄█▀▄▄  ██
  ██▄▄▄▄▄▄▄█▄█▄▄█▄▄▄██▄▄██▄█▄▄██▄██
  ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
  http://192.168.1.20:52814/
  ────────────────────────────────────────────────────────────────────────────
  █████████████████████████████
  ██ ▄▄▄▄▄ ███▀▄▄▄████ ▄▄▄▄▄ ██
  ██ █   █ ███▀▄ ▄▀ ██ █   █ ██
  ██ █▄▄▄█ █ ▄▀  █ ▄▀█ █▄▄▄█ ██
  ██▄▄▄▄▄▄▄██▀▄▄▄▀██▀█▄▄▄▄▄▄▄██
  ██▀▄▀                █ █▄█▄██
  ██▄ ▄▄▄  NOT A REAL  ▄▄ █▀ ██
  █████ █   QR CODE    ▀▀▀▄████
  ██ ▀▄ █              ▄▄██ ▀██
  ██▄▄▄█▄█▄▄▄▄▄▄▄▄██▄██ ▄▄ ▀ ██
  ██ ▄▄▄▄▄ █  █▀▀▄▀▄▀▄▄▀▄▄▀▄ ██
  ██ █   █ ██▀ ▄▀▄ ▀▄███▄ ▀  ██
  ██ █▄▄▄█ ██▀▀▄▄██▄  ▄▄▄█▀▀ ██
  ██▄▄▄▄▄▄▄█▄██▄██▄▄█▄█▄█▄█▄▄██
  ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
  https://192.168.1.20:5273
```

See [Your dev server on your phone](#your-dev-server-on-your-phone) for what
the QR codes do.

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

## Quick start: what do you want to do?

**"I want HTTPS for my dev server on port 5173."**

```sh
ferry 5273:5173
```

`https://localhost:5273` now reaches your app. The certificate comes from
ferry's local CA, so trust that once and browsers stop warning.

**"Can ferry start the dev server too?"**

```sh
ferry 5273:5173 -- npm run dev
```

Everything after `--` is run alongside ferry. Requests wait until the app is
listening, and Ctrl-C stops both. See
[Running your app with ferry](#running-your-app-with-ferry).

**"I want to try it on my phone."**

Same command. Scan the first QR code in the banner to trust ferry's CA (once
per phone), then the second to open the app. See
[Your dev server on your phone](#your-dev-server-on-your-phone).

**"My frontend calls an API server on port 8000."**

```sh
ferry 5273:5173 8443:8000
```

Once the page is HTTPS, browsers block its requests to a plain-HTTP API as
mixed content, so give the API its own HTTPS port and point the frontend at
`https://…:8443`.

**"Only this machine should be able to connect."**

```sh
ferry 127.0.0.1:5273:5173
```

Without an address, ferry listens on every interface, so phones and other
computers can reach it.

**"My app listens on a Unix socket."**

```sh
ferry 9001:./app.sock          # relative to where you run ferry
ferry 9001:/run/app/app.sock   # or absolute
```

**"The app is on another machine or in a container."**

```sh
ferry 5273:10.0.0.5:8080
ferry 5273:http://app:8080
```

**"This is a real server. I have a certificate, and plain HTTP on port 80
should redirect to HTTPS on 443."**

```sh
sudo ferry ./fullchain.pem ./privkey.pem 80/443:3000
```

Port 80 only ever redirects; ferry opens no ports you didn't name. On Linux,
use `CAP_NET_BIND_SERVICE` instead of `sudo`; see the
[systemd example](#systemd).

**"Run it in the background, with logs."**

```sh
ferry -d --pid-file ferry.pid --access-log access.log --error-log error.log 80/443:3000
```

**"I'd rather keep all this in a file."**

```sh
ferry --print-config ./fullchain.pem ./privkey.pem 80/443:3000 > ferry.toml
ferry -t -c ferry.toml    # check it
ferry -c ferry.toml       # run it
```

ferry reloads the file when it changes. See [Config file](#config-file).

**"I'm ready to ship. What would this be in nginx or Caddy?"**

Coming in v0.3: `ferry --print-config nginx` and `--print-config caddy` will
write an equivalent config for the same setup.

## Your dev server on your phone

To test a web app properly on a phone, you need HTTPS the phone actually
trusts. Clicking through a certificate warning isn't enough:

- **Service workers won't register** in Chrome on a certificate error, even
  after you click through. So no offline mode and no install prompt: you
  can't test a PWA.
- **iOS can't fully trust a self-signed certificate.** Safari's
  click-through lasts one session, and Home Screen apps get no click-through
  at all: their requests just fail.
- **Camera, microphone, geolocation, clipboard and push** only work in a
  secure context.

With ferry it's one command:

```sh
ferry 5273:5173 -- npm run dev
```

ferry starts your dev server and waits for it. Then it prints QR codes, as in
the banner at the top of this page: one for setting up the phone, with the
steps beside it, and one for each app.

1. **Once per phone, scan the first code.** It opens a page that serves
   ferry's certificate authority (CA) and walks you through trusting it on
   iOS or Android. It takes about a minute.
2. **Scan the second code** to open your app over HTTPS the phone trusts.
   Service workers register, Add to Home Screen works, and so do the
   secure-context APIs. Hot reload works too, since WebSockets pass through.

From then on that phone trusts every ferry on your computer, without
warnings. When your laptop moves to another network and gets a new address,
ferry issues a new certificate by itself, and the phone still trusts it,
because it trusts the CA rather than one certificate.

**Compared with the usual setup.** Doing this by hand means running mkcert
or Caddy's internal CA and getting the root certificate onto the phone
yourself, by AirDrop, email or a file server. You'd also write an nginx or
Caddy config, look up your LAN address, and make a new certificate whenever
that address changes. ferry does all of that from the one command, and the
QR codes mean you never type an IP address on a phone.

**Safe by default.** ferry's CA can only vouch for local names (`localhost`,
`*.local`) and private addresses. If its key ever leaked, it couldn't be used
to impersonate other websites to your phone (Android ignores this limit; see
[The local CA](#the-local-ca)). The setup page comes over plain HTTP, so check
its fingerprint starts and ends the same as the one on ferry's `cert:` line.

**Try it** with the small PWA in `tools/pwa-test`. It reports whether it's
in a secure context, whether its service worker is running, and whether it's
running from the Home Screen:

```sh
cd tools/pwa-test
ferry 6005:8000 -- python3 -m http.server 8000 --bind 127.0.0.1
```

## Running your app with ferry

Everything after `--` is a command ferry runs alongside itself, usually your
dev server:

```sh
ferry 5273:5173 -- npm run dev
```

- The command shares the terminal: you see its output, and it can read your
  keystrokes (Vite's shortcuts work).
- Requests that arrive before the app is listening wait for it, for up to 30
  seconds, instead of getting an error page. With several mappings, each
  waits only for its own upstream.
- ferry's banner, with its URLs and QR codes, appears as soon as one
  upstream is listening, below the app's own startup output, even if the dev
  server clears the screen. An upstream still silent after 30 seconds gets a
  warning.
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
| `80/443:6006`             | 443, plus 80 redirecting to it | `http://localhost:6006` |

IPv6 addresses go in brackets. A Unix socket path must start with `/`, `./`
or `../`.

**`HTTP/HTTPS` ports.** `80/443:6006` serves HTTPS on 443 and also listens
for plain HTTP on 80, where every request is redirected (308) to the same
URL over HTTPS on 443. Port 80 never serves your app. That's what a public
server usually wants. Any pair works, such as `8080/8443:6006` without root,
and so do bind addresses (`127.0.0.1:80/443:6006`) and the config file
(`listen = "80/443"`). ferry never opens a port you didn't name: `443:6006`
listens on 443 only.

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

The CA is valid for 10 years; ferry makes a new one a month before it
expires. If the stored CA exists but can't be used (say its key isn't
readable), ferry stops with the reason rather than replace it, since devices
that trust it would stop working. It has *name constraints*: it can only vouch
for `localhost`, names ending in `.local`, and loopback and private
addresses. A trusted CA can otherwise vouch for any site, so this limits the
damage if its key ever leaks (Android ignores name constraints, so there the
protection doesn't apply). The certificate covers your machine as
`hostname.local` but not as the bare hostname: use `.local` or an address
from other machines. If your machine has an address the CA can't cover, such
as a public IP, ferry leaves it out of the certificate and warns that
visitors from the internet will get certificate warnings: for a public site,
give ferry a certificate. If you need a CA without constraints on a private
network, set `ca_name_constraints = false` under `[advanced]` and delete
`ca.pem` and `ca-key.pem`; the setting only applies when the CA is created.
Don't do that on a machine the internet can reach: a CA without constraints
can vouch for any site, so its key is worth stealing.

On every start, ferry issues a new server
certificate from it (`server.pem` in the same directory) covering
`localhost`, `127.0.0.1`, `::1`, your hostname (and `hostname.local`), and the
machine's LAN IPv4 addresses. While it runs, it issues another when those
change, such as when your laptop joins another network. Nothing trusts the
server certificate directly, so replacing it costs nothing.

Browsers warn until you trust the CA. ferry serves a page about trusting it
over plain HTTP, on a port of its own that the system picks, so it never
appears at your apps' addresses; the banner links to it. It answers only
devices on this machine and private networks (loopback, private and
carrier-grade NAT addresses, such as Tailscale's); connections from anywhere
else are closed. Opened on this machine, the page has the commands for it,
filled in with the real paths:

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

**Phones.** When stderr is a terminal, the banner shows a QR code for that
page at your LAN address, with the steps beside it, and a QR code for each
app; see [Your dev server on your phone](#your-dev-server-on-your-phone).
Android then shows "Network may be monitored", which is expected for any
certificate you add yourself. If every app that uses the CA listens on
loopback only, phones couldn't reach it anyway, so the page listens on
loopback too. To leave the QR codes out of the banner, set `qr_code = false`
under `[advanced]`.

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
listen   = "80/443"                        # 443, plus 80 redirecting to it
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

- Speaks HTTP/2 and HTTP/1.1 to clients, negotiated automatically (ALPN).
- Forwards the method, path, query and headers unchanged, and keeps `Host`
  as the client sent it. `CONNECT` is refused with `405`.
- Removes hop-by-hop headers (`Connection` and the headers it names, other
  than `Host` and `Content-Length`; `Keep-Alive`, `TE`, `Transfer-Encoding`,
  `Upgrade`, `Proxy-*`).
- Adds `X-Forwarded-For` (appended to any existing value; the last entry is
  the one ferry saw), `X-Real-IP` (the client's address, replacing any the
  client sent), `X-Forwarded-Proto: https`, `X-Forwarded-Host` and
  `X-Forwarded-Port`. A client's `Forwarded` header is dropped.
- Adds an `X-Request-Id` if the client didn't send one, passes it to the
  upstream, returns it in the response, and logs it.
- Streams request and response bodies without buffering them.
- Talks HTTP/1.1 to upstreams, with pooled keep-alive connections.
- Passes `Upgrade` requests (WebSockets) through, in both directions. Clients
  open WebSockets over HTTP/1.1; browsers do this automatically. An upgraded
  connection is closed when nothing has moved either way for `idle_timeout`
  (most WebSocket libraries ping well within that), or when ferry shuts down.
- `--max-body` rejects larger requests with `413`, for both `Content-Length`
  and chunked bodies.
- Answers `502` with an explanation when the upstream can't be reached, and
  `504` when connecting times out or the upstream doesn't start answering
  within `idle_timeout`. Browsers get a small HTML page; other clients get
  plain text. Only clients on this machine or a private network are told
  which upstream failed and why; others get the request ID, which the error
  log line has too.
- Raises its open-file limit at startup, and caps connections (clients and
  upstreams together) to fit it. At the cap, new clients wait to be
  accepted, and requests that need an upstream connection get `503`.
- Closes a connection that sends no request within `handshake_timeout`, one
  with no requests for `idle_timeout` (HTTP/1.1 and HTTP/2 alike), and one
  where a request has stopped moving: no bytes either way for longer than
  that. Slow transfers that keep moving are fine.

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
| HTTP → HTTPS on the same port    | redirect                               | `--no-redirect`, `redirect = false`           |
| Plain HTTP port (e.g. 80 → 443)  | none                                   | `80/443:UPSTREAM`, `listen = "80/443"`        |
| Body size limit                  | none                                   | `--max-body`, `max_body`                      |
| Access log                       | stdout, `combined`                     | `--access-log`, `--log-format`, `-q`          |
| Error log                        | stderr                                 | `--error-log`                                 |
| Upstream connect timeout         | 5s                                     | `[advanced] connect_timeout`                  |
| TLS handshake + first request    | 15s                                    | `[advanced] handshake_timeout`                |
| Idle connections, stalled requests | 90s                                  | `[advanced] idle_timeout`                     |
| Shutdown grace period            | 10s                                    | `[advanced] shutdown_grace`                   |
| Connections open at once         | from the open-file limit               | `[advanced] max_connections`                  |
| QR codes for phones in the banner | on, when stderr is a terminal         | `[advanced] qr_code`                          |
| Local CA name constraints        | on (local names, private addresses)    | `[advanced] ca_name_constraints`              |

## Shell completions and man page

```sh
ferry --completions zsh  > ~/.zfunc/_ferry          # or bash, fish, elvish
ferry --completions bash > /etc/bash_completion.d/ferry
sudo install -m 644 doc/ferry.1 /usr/local/share/man/man1/   # then: man ferry
```

## Development

ferry is developed in a private repository and published here at each
release, so `main` is always the latest release, and every release is a tag.

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
