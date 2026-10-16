//! Command-line argument parsing.

use clap::Parser;

/// A very simple HTTPS reverse proxy.
#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {}
