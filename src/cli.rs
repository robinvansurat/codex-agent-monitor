use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "codex-agent-monitor")]
#[command(about = "Inspect Codex thread activity from persisted state")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(long, global = true, env = "CODEX_HOME")]
    pub codex_home: Option<String>,

    #[arg(long, default_value = "false")]
    pub all: bool,

    #[arg(long)]
    pub project: Option<String>,

    #[arg(long)]
    pub thread: Option<String>,

    #[arg(long)]
    pub state: Option<ThreadStateFilter>,

    #[arg(long, default_value_t = 1440)]
    pub recent: u64,

    #[arg(long)]
    pub depth: Option<usize>,

    #[arg(long)]
    pub role: Option<String>,

    #[arg(long)]
    pub runtime_events: Option<String>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,
    #[arg(long)]
    pub json: bool,

    #[arg(long)]
    pub interval_ms: Option<u64>,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum ThreadStateFilter {
    Running,
    Idle,
    Interrupted,
    Failed,
    Unknown,
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    Probe(ProbeArgs),
    Watch(WatchArgs),
    Tui(TuiArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ProbeArgs {
    #[arg(long, default_value = "false")]
    pub all: bool,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub thread: Option<String>,
    #[arg(long)]
    pub state: Option<ThreadStateFilter>,
    #[arg(long, default_value_t = 1440)]
    pub recent: u64,
    #[arg(long)]
    pub depth: Option<usize>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub runtime_events: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub interval_ms: Option<u64>,
}

#[derive(Args, Debug, Clone)]
pub struct WatchArgs {
    #[arg(long, default_value_t = 2000)]
    pub interval_ms: u64,
    #[arg(long, default_value = "false")]
    pub all: bool,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub thread: Option<String>,
    #[arg(long)]
    pub state: Option<ThreadStateFilter>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub runtime_events: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,
    #[arg(long, default_value_t = 1440)]
    pub recent: u64,
    #[arg(long)]
    pub depth: Option<usize>,
}

#[derive(Args, Debug, Clone)]
pub struct TuiArgs {
    #[arg(long, default_value = "false")]
    pub all: bool,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub thread: Option<String>,
    #[arg(long)]
    pub state: Option<ThreadStateFilter>,
    #[arg(long)]
    pub depth: Option<usize>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub runtime_events: Option<String>,
    #[arg(long)]
    pub recent: Option<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct FilterOpts {
    pub all: bool,
    pub project: Option<String>,
    pub thread: Option<String>,
    pub state: Option<ThreadStateFilter>,
    pub recent_minutes: Option<u64>,
    pub depth: Option<usize>,
    pub role: Option<String>,
    pub runtime_events: Option<String>,
    pub format: OutputFormat,
    pub interval_ms: Option<u64>,
}

pub fn merge_probe_args(cli: &Cli, probe: Option<ProbeArgs>) -> FilterOpts {
    if let Some(p) = probe {
        FilterOpts {
            all: p.all,
            project: p.project.or_else(|| cli.project.clone()),
            thread: p.thread.or_else(|| cli.thread.clone()),
            state: p.state.or_else(|| cli.state.clone()),
            recent_minutes: Some(p.recent),
            depth: p.depth,
            role: p.role.or_else(|| cli.role.clone()),
            runtime_events: p.runtime_events.or_else(|| cli.runtime_events.clone()),
            format: if p.json { OutputFormat::Json } else { p.format },
            interval_ms: p.interval_ms.or(cli.interval_ms),
        }
    } else {
        let mut f = global_filter(cli);
        f.recent_minutes = Some(cli.recent);
        f
    }
}

pub fn merge_watch_args(cli: &Cli, watch: Option<WatchArgs>) -> FilterOpts {
    if let Some(w) = watch {
        FilterOpts {
            all: w.all,
            project: w.project.or_else(|| cli.project.clone()),
            thread: w.thread.or_else(|| cli.thread.clone()),
            state: w.state.or_else(|| cli.state.clone()),
            recent_minutes: Some(w.recent),
            depth: w.depth,
            role: w.role.or_else(|| cli.role.clone()),
            runtime_events: w.runtime_events.or_else(|| cli.runtime_events.clone()),
            format: w.format,
            interval_ms: Some(w.interval_ms),
        }
    } else {
        let mut f = global_filter(cli);
        f.recent_minutes = Some(cli.recent);
        f
    }
}

pub fn merge_tui_args(cli: &Cli, tui: Option<TuiArgs>) -> FilterOpts {
    if let Some(t) = tui {
        FilterOpts {
            all: t.all,
            project: t.project.or_else(|| cli.project.clone()),
            thread: t.thread.or_else(|| cli.thread.clone()),
            state: t.state.or_else(|| cli.state.clone()),
            recent_minutes: t.recent,
            depth: t.depth,
            role: t.role.or_else(|| cli.role.clone()),
            runtime_events: t.runtime_events.or_else(|| cli.runtime_events.clone()),
            format: OutputFormat::Human,
            interval_ms: Some(2000),
        }
    } else {
        let mut f = global_filter(cli);
        f.format = OutputFormat::Human;
        f
    }
}

fn global_filter(cli: &Cli) -> FilterOpts {
    FilterOpts {
        all: cli.all,
        project: cli.project.clone(),
        thread: cli.thread.clone(),
        state: cli.state.clone(),
        recent_minutes: Some(cli.recent),
        depth: cli.depth,
        role: cli.role.clone(),
        runtime_events: cli.runtime_events.clone(),
        format: if cli.json {
            OutputFormat::Json
        } else {
            cli.format.clone()
        },
        interval_ms: cli.interval_ms,
    }
}
