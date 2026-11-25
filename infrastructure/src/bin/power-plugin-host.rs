use anyhow::{Context, Result};
use std::{io, path::PathBuf};

fn main() -> Result<()> {
    let directory = std::env::args_os()
        .nth(1)
        .context("Expected plugin directory")?;
    match infrastructure::plugins::run_host(PathBuf::from(directory)) {
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
