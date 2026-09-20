use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use codex_agent_monitor::cli::{FilterOpts, OutputFormat, Provider};
use codex_agent_monitor::observer::Monitor;
use codex_agent_monitor::runtime::RuntimeOverlay;

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            TOTAL.fetch_add(layout.size(), Ordering::Relaxed);
            let current = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(current, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn native_reader_peak_allocation_is_bounded_when_ignoring_large_arrays() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/large.json"),
        r#"{"session_id":"large","cwd":"/work","session_state":{"agent_name":"default"}}"#,
    );
    let numbers = format!("{}0", "0,".repeat(2_000_000));
    write(
        &temp.path().join("sessions/cli/large.jsonl"),
        &format!(
            "{{\"kind\":\"Prompt\",\"data\":{{\"meta\":{{\"timestamp\":1789210800}},\"content\":[{{\"kind\":\"text\",\"data\":{{\"source\":{{\"data\":[{}]}}}}}}]}}}}\n",
            numbers
        ),
    );

    let mut monitor = Monitor::new_with_sources_and_usage(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        None,
        Provider::Kiro,
        None,
        true,
    )
    .unwrap();
    let filters = FilterOpts {
        provider: Provider::Kiro,
        kiro_home: None,
        kiro_db: None,
        kiro_cli: None,
        no_kiro_usage: true,
        all: true,
        project: None,
        thread: None,
        state: None,
        recent_minutes: None,
        depth: None,
        role: None,
        runtime_events: None,
        format: OutputFormat::Json,
        interval_ms: None,
    };
    let warmup = monitor
        .probe_snapshot(&filters, RuntimeOverlay::default(), true)
        .unwrap();
    assert_eq!(warmup.threads.len(), 1);
    drop(warmup);
    let baseline = CURRENT.load(Ordering::Relaxed);
    let total_baseline = TOTAL.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    for _ in 0..8 {
        let output = monitor
            .probe_snapshot(&filters, RuntimeOverlay::default(), true)
            .unwrap();
        assert_eq!(output.threads.len(), 1);
    }
    let peak_delta = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    let total_delta = TOTAL.load(Ordering::Relaxed).saturating_sub(total_baseline);
    assert!(
        peak_delta < 32 * 1024 * 1024,
        "native reader peak allocation exceeded bound: {} bytes",
        peak_delta
    );
    assert!(
        total_delta < 2 * 1024 * 1024,
        "native reader cumulative refresh allocation exceeded bound: {} bytes",
        total_delta
    );
}
