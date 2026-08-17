use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;

use codex_agent_monitor::cli::{merge_probe_args, merge_tui_args, merge_watch_args, Cli, Command};
use codex_agent_monitor::observer::Monitor;
use codex_agent_monitor::output;
use codex_agent_monitor::runtime::RuntimeOverlay;
use codex_agent_monitor::tui_ui;
use serde_json::to_string_pretty;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut monitor = Monitor::new(cli.codex_home.clone())?;
    let command = cli
        .command
        .clone()
        .unwrap_or(Command::Tui(codex_agent_monitor::cli::TuiArgs {
            all: cli.all,
            project: cli.project.clone(),
            thread: cli.thread.clone(),
            state: cli.state.clone(),
            depth: cli.depth,
            role: cli.role.clone(),
            runtime_events: cli.runtime_events.clone(),
            recent: Some(cli.recent),
        }));

    match command {
        Command::Probe(args) => {
            let opts = merge_probe_args(&cli, Some(args));
            run_probe(&mut monitor, opts)?
        }
        Command::Watch(args) => {
            let opts = merge_watch_args(&cli, Some(args));
            run_watch(&mut monitor, opts)?
        }
        Command::Tui(args) => {
            let opts = merge_tui_args(&cli, Some(args));
            tui_ui::run_tui(&mut monitor, &opts)?;
        }
    }
    Ok(())
}

fn run_probe(monitor: &mut Monitor, opts: codex_agent_monitor::cli::FilterOpts) -> Result<()> {
    let runtime = RuntimeOverlay::from_source(opts.runtime_events.as_deref());
    let snapshot = monitor.probe_snapshot(&opts, runtime, true)?;
    if matches!(opts.format, codex_agent_monitor::cli::OutputFormat::Json) {
        println!("{}", to_string_pretty(&snapshot)?);
    } else {
        println!("{}", output::render_human(&snapshot));
    }
    Ok(())
}

fn run_watch(monitor: &mut Monitor, opts: codex_agent_monitor::cli::FilterOpts) -> Result<()> {
    let interval_ms = opts.interval_ms.unwrap_or(2000);
    let runtime_events = opts.runtime_events.clone();
    if matches!(runtime_events.as_deref(), Some("-")) {
        return Err(anyhow!(
            "--runtime-events - is only supported for one-shot probe"
        ));
    }
    loop {
        let runtime = RuntimeOverlay::from_source(runtime_events.as_deref());
        let snapshot = monitor.probe_snapshot(&opts, runtime, false)?;
        match opts.format {
            codex_agent_monitor::cli::OutputFormat::Json => {
                println!("{}", serde_json::to_string(&snapshot)?);
            }
            codex_agent_monitor::cli::OutputFormat::Human => {
                print!("\x1b[2J\x1b[H");
                println!("{}", output::render_human(&snapshot));
            }
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }
}
