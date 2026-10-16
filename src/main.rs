mod cli;
mod config;
mod log;
mod proxy;
mod tls;

use clap::Parser;

fn main() {
    let _cli = cli::Cli::parse();
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        crate::cli::Cli::command().debug_assert();
    }
}
