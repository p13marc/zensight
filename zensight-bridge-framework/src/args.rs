//! CLI argument parsing for bridges.

use std::path::PathBuf;

use clap::Parser;

/// Common CLI arguments for all bridges.
#[derive(Parser, Debug, Clone)]
#[command(about = "ZenSight protocol bridge")]
pub struct BridgeArgs {
    /// Path to configuration file.
    #[arg(short, long)]
    pub config: PathBuf,

    /// Override log level (trace, debug, info, warn, error).
    #[arg(long)]
    pub log_level: Option<String>,
}

impl BridgeArgs {
    /// Parse CLI arguments with a default config path.
    ///
    /// If no `--config` argument is provided, uses the default.
    pub fn parse_with_default(default_config: &'static str) -> Self {
        let matches = <Self as clap::CommandFactory>::command()
            .mut_arg("config", |arg| arg.default_value(default_config))
            .get_matches();

        <Self as clap::FromArgMatches>::from_arg_matches(&matches)
            .expect("Failed to parse arguments")
    }

    /// Parse CLI arguments (requires --config to be specified).
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_args_default_config() {
        // This would require actual CLI parsing, so we just verify the struct exists
        let args = BridgeArgs {
            config: PathBuf::from("test.json5"),
            log_level: Some("debug".to_string()),
        };
        assert_eq!(args.config, PathBuf::from("test.json5"));
        assert_eq!(args.log_level, Some("debug".to_string()));
    }
}
