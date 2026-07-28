// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![forbid(unsafe_code)]

use solstone_tmux_observer::cli;
use solstone_tmux_observer::clock::SystemClock;
use solstone_tmux_observer::command::TokioCommandRunner;
use solstone_tmux_observer::paths::ProcessEnvironment;
use solstone_tmux_observer::service::{
    ServiceController, current_platform, load_local_observer, status_exit_code,
};
use time::UtcOffset;

fn main() {
    let exit_code = run().unwrap_or_else(|error| {
        eprintln!("solstone-tmux-observer: {error}");
        1
    });
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn run() -> Result<i32, String> {
    let command = match cli::parse_args(std::env::args_os()) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("{error}");
            return Ok(cli::USAGE_EXIT_CODE);
        }
    };

    // This must happen before Tokio can create a worker or driver thread.
    let local_offset = UtcOffset::current_local_offset()
        .map_err(|error| format!("could not determine the local UTC offset at startup: {error}"))?;
    let _clock = SystemClock::new(local_offset);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| format!("could not start the async runtime: {error}"))?;
    let environment = ProcessEnvironment;
    let platform = current_platform();
    let runner = TokioCommandRunner;
    let binary = std::env::current_exe()
        .map_err(|error| format!("could not resolve the observer executable: {error}"))?;
    let service = ServiceController::new(platform, &environment, &runner, binary);

    runtime.block_on(async {
        match command {
            cli::CliCommand::InstallService => {
                service.install().await.map_err(|error| error.to_string())?;
                Ok(0)
            }
            cli::CliCommand::UninstallService => {
                service
                    .uninstall()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(0)
            }
            cli::CliCommand::Status => {
                let status = service.status().await;
                if let Err(error) = &status {
                    eprintln!("solstone-tmux-observer: {error}");
                }
                Ok(status_exit_code(status))
            }
            cli::CliCommand::Run => {
                let state = load_local_observer(platform, &environment)
                    .map_err(|error| error.to_string())?;
                Err(format!(
                    "native run lifecycle is not enabled by the repository install path (resolved tmux: {})",
                    state.tmux_path.display()
                ))
            }
        }
    })
}
