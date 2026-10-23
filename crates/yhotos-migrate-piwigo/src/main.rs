#![forbid(unsafe_code)]
//! Operator-only one-shot Piwigo migration. Credentials are environment-only.
use yhotos_migrate_piwigo::{Error, Options, Result};

fn options() -> Result<Options> {
    let mut args = std::env::args_os().skip(1);
    let mut source_root = None;
    let mut storage_root = None;
    let mut report = None;
    let mut dry_run = false;
    let mut concurrency = 4;
    let mut source_offset = time::UtcOffset::UTC;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--dry-run") => dry_run = true,
            Some("--source-root") => {
                source_root = Some(args.next().ok_or(Error::Configuration)?.into());
            }
            Some("--storage-root") => {
                storage_root = Some(args.next().ok_or(Error::Configuration)?.into());
            }
            Some("--report") => report = Some(args.next().ok_or(Error::Configuration)?.into()),
            Some("--concurrency") => {
                concurrency = args
                    .next()
                    .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
                    .ok_or(Error::Configuration)?;
            }
            Some("--source-offset-seconds") => {
                let seconds = args
                    .next()
                    .and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
                    .ok_or(Error::Configuration)?;
                source_offset = time::UtcOffset::from_whole_seconds(seconds)
                    .map_err(|_| Error::Configuration)?;
            }
            _ => return Err(Error::Configuration),
        }
    }
    Ok(Options {
        source_root: source_root.ok_or(Error::Configuration)?,
        storage_root: storage_root.unwrap_or_default(),
        report: report.ok_or(Error::Configuration)?,
        dry_run,
        concurrency,
        source_offset,
    })
}

#[tokio::main]
#[allow(clippy::print_stdout, clippy::print_stderr)]
async fn main() -> std::process::ExitCode {
    let result = match options() {
        Ok(options) => yhotos_migrate_piwigo::run(&options).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(report) => {
            if let Ok(json) = serde_json::to_string_pretty(&report) {
                println!("{json}");
                std::process::ExitCode::SUCCESS
            } else {
                eprintln!("report serialization failed");
                std::process::ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
