use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "codex-agent-monitor")]
#[command(about = "Inspect Codex, Claude, Kiro, and Ollama thread activity from persisted state")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub shared: SharedArgs,

    #[arg(long, global = true, env = "CODEX_HOME")]
    pub codex_home: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct SharedArgs {
    #[arg(long, value_enum, default_value_t = Provider::All, global = true)]
    pub provider: Provider,
    #[arg(long, global = true, env = "KIRO_HOME")]
    pub kiro_home: Option<String>,
    #[arg(long, global = true)]
    pub kiro_db: Option<String>,
    #[arg(long, global = true)]
    pub ollama_db: Option<String>,
    #[arg(long, global = true, env = "CLAUDE_CONFIG_DIR")]
    pub claude_home: Option<String>,
    #[arg(long, global = true)]
    pub claude_usage_file: Option<String>,
    #[arg(long, global = true)]
    pub kiro_cli: Option<String>,
    #[arg(long, global = true)]
    pub no_kiro_usage: bool,
    #[arg(long, default_value = "false", global = true)]
    pub all: bool,
    #[arg(long, global = true)]
    pub project: Option<String>,
    #[arg(long, global = true)]
    pub thread: Option<String>,
    #[arg(long, global = true)]
    pub state: Option<ThreadStateFilter>,
    #[arg(long, default_value_t = 1440, global = true)]
    pub recent: u64,
    #[arg(long, global = true)]
    pub depth: Option<usize>,
    #[arg(long, global = true)]
    pub role: Option<String>,
    #[arg(long, global = true)]
    pub runtime_events: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human, global = true)]
    pub format: OutputFormat,
    #[arg(long, global = true)]
    pub json: bool,
    #[arg(long, global = true)]
    pub interval_ms: Option<u64>,
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq)]
pub enum ThreadStateFilter {
    Running,
    Idle,
    Unknown,
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq, Default)]
pub enum Provider {
    Codex,
    Claude,
    Kiro,
    Ollama,
    #[default]
    All,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    Probe,
    Watch,
    Tui,
}

#[derive(Debug, Default, Clone)]
pub struct FilterOpts {
    pub provider: Provider,
    pub kiro_home: Option<String>,
    pub kiro_db: Option<String>,
    pub kiro_cli: Option<String>,
    pub no_kiro_usage: bool,
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

pub fn merge_probe_args(cli: &Cli) -> FilterOpts {
    global_filter(cli)
}

pub fn merge_watch_args(cli: &Cli) -> FilterOpts {
    let mut filters = global_filter(cli);
    filters.interval_ms = Some(cli.shared.interval_ms.unwrap_or(2000));
    filters
}

pub fn merge_tui_args(cli: &Cli) -> FilterOpts {
    let mut filters = global_filter(cli);
    filters.interval_ms = Some(cli.shared.interval_ms.unwrap_or(2000));
    filters
}

pub fn selected_command(cli: &Cli) -> Command {
    cli.command.clone().unwrap_or(Command::Tui)
}

fn global_filter(cli: &Cli) -> FilterOpts {
    FilterOpts {
        provider: cli.shared.provider.clone(),
        kiro_home: cli.shared.kiro_home.clone(),
        kiro_db: cli.shared.kiro_db.clone(),
        kiro_cli: cli.shared.kiro_cli.clone(),
        no_kiro_usage: cli.shared.no_kiro_usage,
        all: cli.shared.all,
        project: cli.shared.project.clone(),
        thread: cli.shared.thread.clone(),
        state: cli.shared.state.clone(),
        recent_minutes: Some(cli.shared.recent),
        depth: cli.shared.depth,
        role: cli.shared.role.clone(),
        runtime_events: cli.shared.runtime_events.clone(),
        format: if cli.shared.json {
            OutputFormat::Json
        } else {
            cli.shared.format.clone()
        },
        interval_ms: cli.shared.interval_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_shared_filter(args: &[&str]) {
        let cli = Cli::try_parse_from(args).expect("parse CLI");
        let opts = match cli.command.clone() {
            Some(Command::Probe) => merge_probe_args(&cli),
            Some(Command::Watch) => merge_watch_args(&cli),
            _ => panic!("expected probe or watch"),
        };
        assert!(opts.all);
        assert_eq!(opts.project.as_deref(), Some("repo"));
        assert_eq!(opts.thread.as_deref(), Some("thread"));
        assert_eq!(opts.state, Some(ThreadStateFilter::Running));
        assert_eq!(opts.recent_minutes, Some(7));
        assert_eq!(opts.depth, Some(2));
        assert_eq!(opts.role.as_deref(), Some("worker"));
        assert_eq!(opts.runtime_events.as_deref(), Some("events.jsonl"));
        assert_eq!(opts.format, OutputFormat::Json);
    }

    #[test]
    fn shared_flags_parse_before_probe() {
        assert_shared_filter(&[
            "codex-agent-monitor",
            "--json",
            "--all",
            "--project",
            "repo",
            "--thread",
            "thread",
            "--state",
            "running",
            "--recent",
            "7",
            "--depth",
            "2",
            "--role",
            "worker",
            "--runtime-events",
            "events.jsonl",
            "probe",
        ]);
    }

    #[test]
    fn shared_flags_parse_after_watch_and_json_selects_json() {
        assert_shared_filter(&[
            "codex-agent-monitor",
            "watch",
            "--json",
            "--all",
            "--project",
            "repo",
            "--thread",
            "thread",
            "--state",
            "running",
            "--recent",
            "7",
            "--depth",
            "2",
            "--role",
            "worker",
            "--runtime-events",
            "events.jsonl",
        ]);
    }

    #[test]
    fn no_subcommand_defaults_to_tui_without_discarding_shared_filters() {
        let cli =
            Cli::try_parse_from(["codex-agent-monitor", "--json", "--all"]).expect("parse CLI");
        assert!(matches!(selected_command(&cli), Command::Tui));
        let opts = merge_tui_args(&cli);
        assert!(opts.all);
        assert_eq!(opts.format, OutputFormat::Json);
    }

    #[test]
    fn provider_defaults_to_all_and_allows_explicit_codex() {
        let default_cli = Cli::try_parse_from(["codex-agent-monitor"]).expect("parse default CLI");
        assert_eq!(merge_tui_args(&default_cli).provider, Provider::All);
        assert_eq!(Provider::default(), Provider::All);

        let codex_cli = Cli::try_parse_from(["codex-agent-monitor", "--provider", "codex"])
            .expect("parse explicit Codex provider");
        assert_eq!(merge_tui_args(&codex_cli).provider, Provider::Codex);

        let ollama_cli = Cli::try_parse_from([
            "codex-agent-monitor",
            "--provider",
            "ollama",
            "--ollama-db",
            "/tmp/ollama.sqlite",
            "--kiro-cli",
            "/tmp/kiro-cli",
        ])
        .expect("parse Ollama and Kiro options");
        assert_eq!(merge_tui_args(&ollama_cli).provider, Provider::Ollama);
        assert_eq!(
            ollama_cli.shared.ollama_db.as_deref(),
            Some("/tmp/ollama.sqlite")
        );
        assert_eq!(ollama_cli.shared.kiro_cli.as_deref(), Some("/tmp/kiro-cli"));

        let claude_cli = Cli::try_parse_from([
            "codex-agent-monitor",
            "--provider",
            "claude",
            "--claude-home",
            "/tmp/claude",
        ])
        .expect("parse Claude options");
        assert_eq!(merge_tui_args(&claude_cli).provider, Provider::Claude);
        assert_eq!(
            claude_cli.shared.claude_home.as_deref(),
            Some("/tmp/claude")
        );
    }
}
