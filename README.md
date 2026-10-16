# ferry

A very simple HTTPS reverse proxy for macOS and Linux. Point it at a plain-HTTP
app and get HTTPS in front of it:

```sh
ferry 6005:6006            # https://localhost:6005 → http://localhost:6006
```

> **Status:** early development — nothing works yet. See the v0.1 milestone.

## Principle

**Do as much as possible by default.** Running ferry with no configuration
should cover 99% of use cases. Prefer a working default over a new flag, and
keep rarely-changed knobs in the config file rather than on the command line.

## Building

Requires Rust 1.85 or newer.

```sh
cargo build --release      # binary at target/release/ferry
```

Before sending changes:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
