mod cli;
mod config;
mod log;
mod mapping;
mod proxy;
mod tls;

use clap::{CommandFactory, Parser, error::ErrorKind};

fn main() {
    let cli = cli::Cli::parse();
    let positionals = match cli.positionals() {
        Ok(p) => p,
        Err(msg) => cli::Cli::command()
            .error(ErrorKind::ValueValidation, msg)
            .exit(),
    };

    // TODO(#5): serve instead of describing the plan.
    match &positionals.cert_key {
        Some(ck) => eprintln!("cert: {}  key: {}", ck.cert.display(), ck.key.display()),
        None => eprintln!("cert: self-signed"),
    }
    for m in &positionals.mappings {
        eprintln!("  https :{}  ->  {}", m.listen_display(), m.upstream);
    }
    eprintln!("ferry: proxying is not implemented yet");
}
