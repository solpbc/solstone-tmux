// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![forbid(unsafe_code)]

use solstone_tmux_observer::cli;
use std::sync::Arc;

use solstone_tmux_observer::clock::{Clock, SystemClock};
use solstone_tmux_observer::command::TokioCommandRunner;
use solstone_tmux_observer::config::{RuntimeConfig, system_hostname};
use solstone_tmux_observer::indicator::{CommandIndicatorIo, IndicatorOwnership};
use solstone_tmux_observer::instance_lock::InstanceLock;
use solstone_tmux_observer::observer::{
    ObserverConfig, SegmentManager, production_shutdown_future, run_observer, stream_directory,
    supervise_observer,
};
use solstone_tmux_observer::paths::{
    ProcessEnvironment, ensure_private_directory, resolve_config_root, resolve_data_root,
};
use solstone_tmux_observer::recovery::{RecoveryAction, recover_configured_streams};
use solstone_tmux_observer::segment::SegmentState;
use solstone_tmux_observer::service::{
    ServiceController, current_platform, load_local_observer, status_exit_code,
};
use solstone_tmux_observer::tmux::TmuxAdapter;
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

    let environment = ProcessEnvironment;
    let platform = current_platform();
    match command {
        cli::CliCommand::Run => {
            // This must happen before Tokio can create a worker or driver thread.
            let local_offset = UtcOffset::current_local_offset().map_err(|error| {
                format!("could not determine the local UTC offset at startup: {error}")
            })?;
            run_native(platform, &environment, SystemClock::new(local_offset))
        }
        command => {
            let runtime = runtime()?;
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
                    cli::CliCommand::Run => unreachable!("run was dispatched before service setup"),
                }
            })
        }
    }
}

fn run_native(
    platform: solstone_tmux_observer::paths::PlatformKind,
    environment: &ProcessEnvironment,
    clock: SystemClock,
) -> Result<i32, String> {
    let data_root = resolve_data_root(platform, environment).map_err(|error| error.to_string())?;
    ensure_private_directory(&data_root).map_err(|error| error.to_string())?;
    let instance_lock = InstanceLock::acquire(&data_root).map_err(|error| error.to_string())?;

    let config_root =
        resolve_config_root(platform, environment).map_err(|error| error.to_string())?;
    ensure_private_directory(&config_root).map_err(|error| error.to_string())?;
    let hostname = system_hostname().map_err(|error| error.to_string())?;
    let config = RuntimeConfig::load(&config_root, &hostname).map_err(|error| error.to_string())?;
    let local_observer = load_local_observer(&config_root).map_err(|error| error.to_string())?;
    let provider = Arc::new(
        TmuxAdapter::new(local_observer.tmux_path.clone(), TokioCommandRunner)
            .map_err(|error| error.to_string())?,
    );
    let indicator_io = CommandIndicatorIo::new(TokioCommandRunner, local_observer.tmux_path)
        .map_err(|error| error.to_string())?;

    for record in recover_configured_streams(&instance_lock, &data_root, &config.stream)
        .map_err(|error| error.to_string())?
    {
        if matches!(
            record.action,
            RecoveryAction::Retain | RecoveryAction::Quarantine | RecoveryAction::Failed
        ) {
            eprintln!(
                "solstone-tmux-observer: warning: recovery {:?} for {}: {}",
                record.action,
                record.candidate.display(),
                record.detail
            );
        }
    }

    let runtime = runtime()?;
    let shutdown = runtime
        .block_on(async { production_shutdown_future() })
        .map_err(|error| error.to_string())?;
    let clock: Arc<dyn Clock> = Arc::new(clock);
    let wall_now = clock.wall_now();
    let monotonic_now = clock.monotonic_now();
    let stream_dir = stream_directory(&data_root, &config.stream, wall_now, clock.local_offset())
        .map_err(|error| error.to_string())?;
    let mut segment =
        SegmentState::create(&stream_dir, wall_now, monotonic_now, clock.local_offset())
            .map_err(|error| error.to_string())?;
    let indicator = match runtime.block_on(IndicatorOwnership::install_default(indicator_io)) {
        Ok(indicator) => indicator,
        Err(error) => {
            return startup_error(&mut segment, error.to_string());
        }
    };
    let manager = SegmentManager::new(segment, data_root, config.stream, clock.local_offset());
    let observer = run_observer(
        provider,
        Box::new(manager),
        Box::new(indicator),
        Box::new(instance_lock),
        clock,
        shutdown,
        ObserverConfig {
            capture_interval: config.capture_interval,
            segment_interval: config.segment_interval,
        },
    );
    let exit = runtime.block_on(supervise_observer(observer));
    for failure in &exit.failures {
        eprintln!("solstone-tmux-observer: {failure}");
    }
    Ok(exit.exit_code)
}

fn startup_error(segment: &mut SegmentState, error: String) -> Result<i32, String> {
    match segment.remove_confirmed_empty() {
        Ok(_) => Err(error),
        Err(cleanup) => Err(format!(
            "{error}; removing the empty startup segment also failed: {cleanup}"
        )),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| format!("could not start the async runtime: {error}"))
}
