use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use clap::{CommandFactory, Parser};

    use super::Cli;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_prints_the_package_version() {
        let err = Cli::try_parse_from(["copper-dictate", "--version"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        assert_eq!(
            err.to_string(),
            format!("copper-dictate {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}
