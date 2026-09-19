//! CLI argument parsing for sensors.

use std::path::PathBuf;

use clap::Parser;

/// Common CLI arguments for all sensors.
#[derive(Parser, Debug, Clone)]
#[command(about = "ZenSight protocol sensor")]
pub struct SensorArgs {
    /// Path to configuration file.
    #[arg(short, long)]
    pub config: PathBuf,

    /// Override log level (trace, debug, info, warn, error).
    #[arg(long)]
    pub log_level: Option<String>,

    /// Parse and validate the config, print the verdict, and exit — open no
    /// session, join no fleet, poll nothing (#1150).
    ///
    /// A deploy script gates on the exit status: `0` the config is good, `1` it
    /// is not, with the reason on stderr. Before this the only way to find out
    /// was to start the sensor on the host and read the logs, by which time it
    /// had already joined the bus.
    #[arg(long)]
    pub check_config: bool,
}

impl SensorArgs {
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

/// What `--check-config` prints when the config loaded and validated (#1150).
///
/// It goes to **stdout** and names the file, because a deploy script that
/// checks six sensors wants six lines it can read, and because the exit status
/// is the thing it gates on. A failure is the load error on stderr and a
/// non-zero exit, which every caller already gets from `load()` returning
/// `Err`.
pub fn report_config_ok(path: &std::path::Path) {
    println!("config ok: {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_args_default_config() {
        // This would require actual CLI parsing, so we just verify the struct exists
        let args = SensorArgs {
            config: PathBuf::from("test.json5"),
            log_level: Some("debug".to_string()),
            check_config: false,
        };
        assert_eq!(args.config, PathBuf::from("test.json5"));
        assert_eq!(args.log_level, Some("debug".to_string()));
        assert!(!args.check_config, "checking is opt-in; the default runs");
    }

    #[test]
    fn check_config_is_a_flag_every_sensor_accepts() {
        // The flag lives on the shared args so a deploy script can gate on
        // `--check-config` without knowing which sensor it is calling.
        let args = <SensorArgs as clap::Parser>::try_parse_from([
            "sensor",
            "--config",
            "x.json5",
            "--check-config",
        ])
        .expect("the flag parses");
        assert!(args.check_config);
    }
}
