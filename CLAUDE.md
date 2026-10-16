# ferry

A very simple HTTPS reverse proxy CLI in Rust, for macOS and Linux. See
README.md for what it does from a user's point of view.

## Principle

**Do as much as possible by default.** Zero configuration should cover 99% of
uses. Prefer a working default to a new flag. Keep rarely changed settings in
the config file's `[advanced]` table, not on the command line. Push back on
features that would turn ferry into nginx.

## Commands

```sh
cargo build
cargo test                                   # unit + end-to-end tests
cargo fmt --check
cargo clippy --all-targets -- -D warnings    # must be clean
```

All four must pass before every commit.

## Layout

- `main.rs`: startup order (config → error log → build plan → bind → pid
  file → banner → daemonize → tokio runtime), plus the signal and reload loop.
- `cli.rs`: clap definition. Positional args are sorted into cert/key files
  and mappings.
- `mapping.rs`: `LISTEN:UPSTREAM` parsing (`parse_listen`, `parse_upstream`
  are shared with the config file).
- `config.rs`: `Config` (resolved), TOML loading with file:line errors,
  CLI-over-file merging, `--print-config` output.
- `server.rs`: listeners keyed by (bind, port); state behind `ArcSwap` so
  reloads swap it in place; TLS-vs-plain detection by peeking the first byte;
  idle and shutdown handling.
- `proxy.rs`: request forwarding, headers, upgrades, error pages, body limit.
- `upstream.rs`: hyper-util pooled clients for TCP and Unix sockets.
- `tls.rs`, `selfsigned.rs`: cert loading and checks, and the persisted
  self-signed default.
- `log.rs`: access log (written when the response body finishes) and error
  log (tracing). `log::timestamp` is the one timestamp format: ISO 8601 UTC
  with milliseconds.
- `date.rs`: UTC civil-date arithmetic (days ↔ y/m/d) and formatting. No
  time zones: ferry writes everything in UTC.
- `check.rs` (`-t`), `banner.rs`, `watch.rs` (reload triggers), `daemon.rs`
  (`-d`, pid file), `net.rs` (binding).

## Conventions

- **Errors are for people.** They should say what's wrong and what to do.
  Config loading and `-t` collect *every* error, not just the first.
- **Crypto backend is ring only.** Never enable aws-lc-rs in any crate
  (rustls, tokio-rustls, rcgen, quinn): it breaks easy static musl builds.
- MSRV is 1.88 (`rust-version` in Cargo.toml). Clippy enforces it.
- Paths: everything in a config file resolves relative to the file's
  directory; CLI paths resolve against the starting directory (`cwd` is
  passed to `Config::load_in`, because `-d` changes directory).
- When behaviour or flags change, update **README.md**, **doc/ferry.1** and
  **ferry.example.toml** in the same commit. Tests enforce some of this: every
  CLI flag must appear in the man page, the example config must load, and
  `--print-config` output must load back to an identical config. New config
  keys need adding to that round-trip.

## Tests

- End-to-end tests live in `tests/` and run the real binary via
  `tests/common/mod.rs`: a test upstream with routes `/headers`, `/echo`,
  `/slow` and `/upgrade`, `Ferry::start`, and HTTPS clients that accept any
  cert.
- The harness points `HOME` and `XDG_DATA_HOME` at a temp dir. Tests must
  never touch the real self-signed cert or data directory.
- Prefer an end-to-end test for anything a user would notice.
- Only macOS has been tested so far. Linux is a target but hasn't been built
  or run yet; be careful with platform-specific behaviour (sockets,
  signals, file watching).

## Workflow

- Issues and milestones are on Gitea; use the `tea` CLI (see the gitea skill).
- Work goes straight to `main`, with no branches or PRs. Make one commit per
  story (or per tightly coupled group) with `Closes #N` lines, then push.
- `planning/` is gitignored and holds local research docs, such as
  `dependency-review.md` and `http3-research.md`. Read them, but don't commit
  them.
