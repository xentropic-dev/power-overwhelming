use anyhow::{Context, Result};
use std::{io, path::PathBuf};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let directory = PathBuf::from(args.next().context("Expected plugin directory")?);
    let result = if args.next().is_some_and(|arg| arg == "--status-bar") {
        infrastructure::plugins::run_status_bar_host(directory)
    } else {
        infrastructure::plugins::run_host(directory)
    };
    match result {
        Err(error)
            if error.downcast_ref::<io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                )
            }) =>
        {
            Ok(())
        }
        result => result,
    }
}
