//! Read-only Kiro account balance lookup through the native ACP command.
//!
//! The monitor sends only initialize and `_kiro/account/getUsage`. It does not
//! create a session, load a project, authenticate, or forward callbacks.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::{json, Value};

use crate::model::{
    Confidence, KiroAccountUsage, KiroAddOnCredit, KiroBonusCredit, KiroCreditBalance, Observed,
};

pub const USAGE_SOURCE: &str = "kiro.account.getUsage";
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

type UsageTask = (
    mpsc::Receiver<Result<Observed<KiroAccountUsage>, String>>,
    mpsc::Sender<()>,
    JoinHandle<()>,
);

#[derive(Debug, Clone)]
pub struct UsageClient {
    executable: Option<PathBuf>,
}

impl UsageClient {
    pub fn new(executable: Option<PathBuf>) -> Self {
        Self { executable }
    }

    pub fn is_available(&self) -> bool {
        self.executable.is_some()
    }

    pub fn spawn(&self) -> Option<UsageTask> {
        let executable = self.executable.clone()?;
        let (sender, receiver) = mpsc::channel();
        let (cancel_sender, cancel_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let result = fetch_usage(&executable, REQUEST_TIMEOUT, &cancel_receiver)
                .map_err(|err| safe_error(&err.to_string()));
            let _ = sender.send(result);
        });
        Some((receiver, cancel_sender, handle))
    }
}

pub fn discover_executable(explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(value) = explicit {
        return find_command(value);
    }
    find_command("kiro-cli").or_else(|| {
        dirs::home_dir().and_then(|home| {
            let candidate = home.join(".local").join("bin").join(executable_name());
            candidate.is_file().then_some(candidate)
        })
    })
}

pub fn unknown(detail: impl Into<String>) -> Observed<KiroAccountUsage> {
    Observed {
        value: None,
        source: Some(crate::model::EvidenceSource {
            kind: USAGE_SOURCE.to_string(),
            detail: Some("native Kiro account lookup".to_string()),
        }),
        observed_at: None,
        confidence: Confidence::Low,
        detail: Some(detail.into()),
    }
}

fn fetch_usage(
    executable: &Path,
    timeout: Duration,
    cancel: &mpsc::Receiver<()>,
) -> Result<Observed<KiroAccountUsage>> {
    let mut command = Command::new(executable);
    command
        .args(["acp", "--agent-engine", "v3", "--auth-method", "cli"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|err| anyhow!("unable to start Kiro account lookup: {err}"))?;
    let mut guard = ChildGuard::new(child);

    let mut stdin = guard
        .child
        .as_mut()
        .and_then(|child| child.stdin.take())
        .ok_or_else(|| anyhow!("Kiro account lookup stdin unavailable"))?;
    let stdout = guard
        .child
        .as_mut()
        .and_then(|child| child.stdout.take())
        .ok_or_else(|| anyhow!("Kiro account lookup stdout unavailable"))?;
    let (sender, receiver) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(Ok(line)) = lines.next() {
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let result = (|| {
        write_request(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {"name": "codex-agent-monitor", "version": env!("CARGO_PKG_VERSION")}
                }
            }),
        )?;
        let initialize = wait_for_response(cancel, &receiver, &mut stdin, 1, deadline)
            .map_err(|err| anyhow!("initialize: {err}"))?;
        validate_initialize_response(&initialize)?;
        write_request(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "_kiro/account/getUsage",
                "params": {}
            }),
        )?;
        let response = wait_for_response(cancel, &receiver, &mut stdin, 2, deadline)
            .map_err(|err| anyhow!("getUsage: {err}"))?;
        parse_usage_response_line(&response)
    })();
    drop(stdin);
    if result.is_err() {
        guard.abort();
    } else {
        guard.finish(deadline, cancel)?;
    }
    let _ = reader.join();
    result
}

fn write_request(stdin: &mut impl Write, request: &Value) -> Result<()> {
    serde_json::to_writer(&mut *stdin, request)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    Ok(())
}

fn wait_for_response(
    cancel: &mpsc::Receiver<()>,
    receiver: &mpsc::Receiver<String>,
    stdin: &mut impl Write,
    expected_id: i64,
    deadline: Instant,
) -> Result<String> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("Kiro account lookup timed out"));
        }
        let line = match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.try_recv().is_ok() {
                    return Err(anyhow!("Kiro account lookup cancelled"));
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("Kiro account lookup timed out"));
            }
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if message.get("id").is_some() {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": message.get("id").cloned().unwrap_or(Value::Null),
                    "error": {"code": -32601, "message": "Method not supported"}
                });
                let _ = write_request(stdin, &response);
            }
            let _ = method;
            continue;
        }
        if message.get("id").and_then(Value::as_i64) == Some(expected_id) {
            return Ok(line);
        }
    }
}

fn validate_initialize_response(line: &str) -> Result<()> {
    let message = serde_json::from_str::<Value>(line)?;
    let result = message.get("result");
    if message.get("error").is_some()
        || result.is_none()
        || result
            .and_then(|value| value.get("success"))
            .and_then(Value::as_bool)
            == Some(false)
    {
        return Err(anyhow!("Kiro initialize failed"));
    }
    Ok(())
}

struct ChildGuard {
    child: Option<std::process::Child>,
}

impl ChildGuard {
    fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn finish(&mut self, deadline: Instant, cancel: &mpsc::Receiver<()>) -> Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        while deadline > Instant::now() {
            if cancel.try_recv().is_ok() {
                self.abort();
                return Ok(());
            }
            let finished = match self.child.as_mut() {
                Some(child) => {
                    let pid = child.id();
                    let finished = child.try_wait()?.is_some();
                    if finished {
                        kill_process_group(pid);
                    }
                    finished
                }
                None => true,
            };
            if finished {
                self.child = None;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.abort();
        Ok(())
    }

    fn abort(&mut self) {
        if let Some(child) = self.child.as_mut() {
            kill_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
    }
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.abort();
    }
}

fn parse_usage_response_line(line: &str) -> Result<Observed<KiroAccountUsage>> {
    let message = serde_json::from_str::<Value>(line)?;
    let result = message
        .get("result")
        .filter(|value| value.get("success").and_then(Value::as_bool) == Some(true))
        .ok_or_else(|| anyhow!("Kiro account lookup returned no successful result"))?;
    let data = result
        .get("data")
        .filter(|value| value.is_object())
        .ok_or_else(|| anyhow!("Kiro account lookup returned no usage data"))?;
    let usage = parse_usage_data(data)?;
    Ok(Observed {
        value: Some(usage),
        source: Some(crate::model::EvidenceSource {
            kind: USAGE_SOURCE.to_string(),
            detail: Some("native Kiro account lookup".to_string()),
        }),
        observed_at: Some(Utc::now()),
        confidence: Confidence::Medium,
        detail: Some("observed from Kiro account credits".to_string()),
    })
}

fn parse_usage_data(data: &Value) -> Result<KiroAccountUsage> {
    let plan_credits = data
        .get("usageBreakdowns")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                if item.get("resourceType").and_then(Value::as_str) != Some("CREDIT")
                    || item.get("hasLimit").and_then(Value::as_bool) != Some(true)
                {
                    return None;
                }
                let used = finite_nonnegative(item.get("used")?)?;
                let total = finite_nonnegative(item.get("limit")?)?;
                Some(KiroCreditBalance {
                    used,
                    total,
                    remaining: (total - used).max(0.0),
                })
            })
        });
    if plan_credits.is_none() {
        return Err(anyhow!("Kiro account usage has no valid credit limit"));
    }

    let bonus_credits = data
        .get("bonusCredits")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let used = finite_nonnegative(item.get("used")?)?;
                    let total = finite_nonnegative(item.get("total")?)?;
                    let days_until_expiry = item.get("daysUntilExpiry").and_then(Value::as_i64);
                    if days_until_expiry.is_some_and(|days| days < 0) {
                        return None;
                    }
                    Some(KiroBonusCredit {
                        name: item.get("name").and_then(Value::as_str).map(str::to_string),
                        used,
                        total,
                        remaining: (total - used).max(0.0),
                        days_until_expiry,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let add_on_credits = data
        .get("addOnCredits")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let used = finite_nonnegative(item.get("used")?)?;
                    let total = finite_nonnegative(item.get("total")?)?;
                    Some(KiroAddOnCredit {
                        used,
                        total,
                        remaining: (total - used).max(0.0),
                        expires_at: item
                            .get("expiresAt")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        is_active: item.get("isActive").and_then(Value::as_bool),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(KiroAccountUsage {
        plan_name: data
            .get("planName")
            .and_then(Value::as_str)
            .map(str::to_string),
        billing_cycle_reset: data
            .get("billingCycleReset")
            .and_then(Value::as_str)
            .map(str::to_string),
        plan_credits,
        bonus_credits,
        add_on_credits,
    })
}

fn finite_nonnegative(value: &Value) -> Option<f64> {
    let number = value.as_f64()?;
    number.is_finite().then_some(number).filter(|v| *v >= 0.0)
}

fn find_command(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file().then_some(path.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        let candidate = directory.join(command);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate = directory.join(format!("{command}.exe"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "kiro-cli.exe"
    } else {
        "kiro-cli"
    }
}

fn safe_error(value: &str) -> String {
    if value.contains("timed out") {
        "native lookup timed out".to_string()
    } else if value.contains("start") {
        "native CLI unavailable".to_string()
    } else {
        "native lookup unavailable".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[cfg(unix)]
    fn executable_script(body: &str) -> tempfile::NamedTempFile {
        use std::os::unix::fs::PermissionsExt;
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(file.path()).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(file.path(), permissions).unwrap();
        file
    }

    #[test]
    fn parses_credit_math_and_optional_extras() {
        let value = parse_usage_data(&json!({
            "planName": "KIRO PRO",
            "billingCycleReset": "2026-10-01",
            "usageBreakdowns": [{"resourceType":"OTHER","hasLimit":true,"used":1,"limit":2},{"resourceType":"CREDIT","hasLimit":true,"used":199.67,"limit":1000}],
            "bonusCredits": [{"name":"promo","used":1.25,"total":4.5,"daysUntilExpiry":7}],
            "addOnCredits": [{"used":0,"total":2.5,"expiresAt":null,"isActive":true}]
        }))
        .unwrap();
        assert_eq!(value.plan_credits.unwrap().remaining, 800.33);
        assert_eq!(value.bonus_credits[0].remaining, 3.25);
        assert_eq!(value.add_on_credits[0].remaining, 2.5);
    }

    #[test]
    fn rejects_missing_or_invalid_credit_breakdown() {
        assert!(parse_usage_data(&json!({"usageBreakdowns":[]})).is_err());
        assert!(parse_usage_data(&json!({"usageBreakdowns":[{"resourceType":"CREDIT","hasLimit":true,"used":-1,"limit":2}]})).is_err());
        assert!(parse_usage_data(&json!({"usageBreakdowns":[{"resourceType":"CREDIT","hasLimit":false,"used":1,"limit":2}]})).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_protocol_waits_for_initialize_and_handles_notifications() {
        let script = executable_script(
            r#"
read initialize
printf '%s\n' '{"jsonrpc":"2.0","method":"status","params":{}}'
printf '%s\n' '{"jsonrpc":"2.0","id":99,"method":"fs/read","params":{}}'
read callback_response
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"success":true,"data":{"planName":"KIRO PRO","billingCycleReset":"2026-10-01","usageBreakdowns":[{"resourceType":"CREDIT","hasLimit":true,"used":199.67,"limit":1000}],"bonusCredits":[],"addOnCredits":[]}}}'
"#,
        );
        let (_cancel_sender, cancel_receiver) = mpsc::channel();
        let observed =
            fetch_usage(script.path(), Duration::from_secs(3), &cancel_receiver).unwrap();
        assert_eq!(
            observed.value.unwrap().plan_credits.unwrap().remaining,
            800.33
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_protocol_timeout_kills_child() {
        let script = executable_script("sleep 5");
        let started = Instant::now();
        let (_cancel_sender, cancel_receiver) = mpsc::channel();
        let error =
            fetch_usage(script.path(), Duration::from_millis(100), &cancel_receiver).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn native_protocol_does_not_query_after_failed_initialize() {
        let script = executable_script(
            r#"
read initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"success":false}}'
sleep 5
"#,
        );
        let (_cancel_sender, cancel_receiver) = mpsc::channel();
        let error =
            fetch_usage(script.path(), Duration::from_secs(2), &cancel_receiver).unwrap_err();
        assert!(error.to_string().contains("initialize"));
    }

    #[cfg(unix)]
    #[test]
    fn inherited_stdout_descendant_is_killed_after_success() {
        let script = executable_script(
            r#"
read initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
read usage
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"success":true,"data":{"usageBreakdowns":[{"resourceType":"CREDIT","hasLimit":true,"used":1.5,"limit":2}],"bonusCredits":[],"addOnCredits":[]}}}'
sleep 5 &
exit 0
"#,
        );
        let (_cancel_sender, cancel_receiver) = mpsc::channel();
        let started = Instant::now();
        let observed =
            fetch_usage(script.path(), Duration::from_secs(3), &cancel_receiver).unwrap();
        assert_eq!(observed.value.unwrap().plan_credits.unwrap().remaining, 0.5);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
