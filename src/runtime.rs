use std::time::{Duration, Instant};

use grammers_client::{Client, tl};

use crate::{
    commands::Action,
    help::{Response, render},
};

pub struct RuntimeState {
    started_at: Instant,
    recognized_commands: u64,
}

impl RuntimeState {
    pub fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            recognized_commands: 0,
        }
    }

    pub async fn execute(
        &mut self,
        client: &Client,
        action: &Action,
        message_id: i32,
        prefix: &str,
    ) -> Response {
        self.recognized_commands = self.recognized_commands.saturating_add(1);
        match action {
            Action::Ping => match telegram_ping(client, message_id).await {
                Ok(latency) => Response::plain(format!("🏓 Pong: {}", format_latency(latency))),
                Err(error) => {
                    log_ping_failure(action, message_id, &error);
                    Response::plain("⚠️ Telegram ping failed")
                }
            },
            Action::Stats => {
                let telegram = match telegram_ping(client, message_id).await {
                    Ok(latency) => format_latency(latency),
                    Err(error) => {
                        log_ping_failure(action, message_id, &error);
                        "unavailable".to_owned()
                    }
                };
                let proc_stats = read_proc_stats().await;
                log_unavailable_proc_stats(&proc_stats);
                Response::plain(format_stats(
                    &telegram,
                    self.started_at.elapsed(),
                    &proc_stats,
                    self.recognized_commands,
                ))
            }
            Action::Help(request) => {
                let rendered = render(request, prefix);
                if rendered.entity_fallback {
                    tracing::warn!(
                        event = "help_entity_fallback",
                        "Help formatting was unavailable"
                    );
                }
                rendered.response
            }
        }
    }
}

async fn telegram_ping(
    client: &Client,
    message_id: i32,
) -> Result<Duration, grammers_mtsender::InvocationError> {
    let started_at = Instant::now();
    client
        .invoke(&tl::functions::Ping {
            ping_id: i64::from(message_id),
        })
        .await?;
    Ok(started_at.elapsed())
}

fn log_ping_failure(action: &Action, message_id: i32, error: &grammers_mtsender::InvocationError) {
    tracing::warn!(
        event = "telegram_ping_failed",
        command = action.name(),
        message_id,
        error_category = invocation_error_category(error),
        "Telegram ping failed"
    );
}

pub(crate) fn invocation_error_category(
    error: &grammers_mtsender::InvocationError,
) -> &'static str {
    match error {
        grammers_mtsender::InvocationError::Session(_) => "session",
        grammers_mtsender::InvocationError::Rpc(_) => "rpc",
        grammers_mtsender::InvocationError::Io(_) => "io",
        grammers_mtsender::InvocationError::Deserialize(_) => "deserialize",
        grammers_mtsender::InvocationError::Transport(_) => "transport",
        grammers_mtsender::InvocationError::Dropped => "dropped",
        grammers_mtsender::InvocationError::InvalidDc => "invalid_dc",
        grammers_mtsender::InvocationError::Authentication(_) => "authentication",
    }
}

#[derive(Debug, Default)]
struct ProcStats {
    system_uptime: Option<Duration>,
    memory_kib: Option<u64>,
}

async fn read_proc_stats() -> ProcStats {
    tokio::task::spawn_blocking(|| ProcStats {
        system_uptime: std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|uptime| parse_system_uptime(&uptime)),
        memory_kib: std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| parse_memory_kib(&status)),
    })
    .await
    .unwrap_or_default()
}

fn log_unavailable_proc_stats(proc_stats: &ProcStats) {
    if proc_stats.system_uptime.is_none() {
        tracing::debug!(
            event = "proc_stat_unavailable",
            stat = "system_uptime",
            "Proc stat unavailable"
        );
    }
    if proc_stats.memory_kib.is_none() {
        tracing::debug!(
            event = "proc_stat_unavailable",
            stat = "memory",
            "Proc stat unavailable"
        );
    }
}

fn parse_system_uptime(input: &str) -> Option<Duration> {
    let seconds = input.split_whitespace().next()?.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds >= 0.0)
        .then_some(seconds)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
}

fn parse_memory_kib(input: &str) -> Option<u64> {
    input.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("VmRSS:") {
            return None;
        }
        let value = fields.next()?;
        (fields.next() == Some("kB"))
            .then(|| value.parse().ok())
            .flatten()
    })
}

fn format_latency(latency: Duration) -> String {
    if latency < Duration::from_millis(1) {
        "<1 ms".to_owned()
    } else {
        format!("{} ms", latency.as_millis())
    }
}

fn format_duration(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3_600;
    seconds %= 3_600;
    let minutes = seconds / 60;
    seconds %= 60;

    if days > 0 {
        format!("{days}d {hours:02}h {minutes:02}m {seconds:02}s")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_stats(
    telegram: &str,
    lavis_uptime: Duration,
    proc_stats: &ProcStats,
    recognized_commands: u64,
) -> String {
    let system_uptime = proc_stats
        .system_uptime
        .map(format_duration)
        .unwrap_or_else(|| "unavailable".to_owned());
    let memory = proc_stats
        .memory_kib
        .map(|memory_kib| format!("{:.1} MiB RSS", memory_kib as f64 / 1024.0))
        .unwrap_or_else(|| "unavailable".to_owned());

    format!(
        "📊 Lavis stats\n\nTelegram: {telegram}\nLavis uptime: {}\nSystem uptime: {system_uptime}\nMemory: {memory}\nCommands: {recognized_commands}\nVersion: {}",
        format_duration(lavis_uptime),
        env!("CARGO_PKG_VERSION"),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ProcStats, format_duration, format_latency, format_stats, parse_memory_kib,
        parse_system_uptime,
    };
    use std::time::Duration;

    #[test]
    fn formats_durations_compactly() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
        assert_eq!(format_duration(Duration::from_millis(999)), "0s");
        assert_eq!(format_duration(Duration::from_secs(61)), "1m 01s");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "1h 01m 01s");
        assert_eq!(
            format_duration(Duration::from_secs(183_845)),
            "2d 03h 04m 05s"
        );
    }

    #[test]
    fn formats_latency_in_milliseconds() {
        assert_eq!(format_latency(Duration::ZERO), "<1 ms");
        assert_eq!(format_latency(Duration::from_micros(999)), "<1 ms");
        assert_eq!(format_latency(Duration::from_millis(12)), "12 ms");
    }

    #[test]
    fn parses_valid_system_uptime() {
        assert_eq!(parse_system_uptime("0.00 0.00"), Some(Duration::ZERO));
        assert_eq!(
            parse_system_uptime("61.42 120.00"),
            Some(Duration::from_secs_f64(61.42))
        );
        assert_eq!(
            parse_system_uptime("183845.75 999999.99"),
            Some(Duration::from_secs_f64(183845.75))
        );
    }

    #[test]
    fn rejects_malformed_system_uptime() {
        assert_eq!(parse_system_uptime(""), None);
        assert_eq!(parse_system_uptime("NaN 1.0"), None);
        assert_eq!(parse_system_uptime("-1 1.0"), None);
        assert_eq!(parse_system_uptime("invalid"), None);
    }

    #[test]
    fn parses_memory_kib_with_extra_whitespace() {
        assert_eq!(
            parse_memory_kib("Name:\tlavis\nVmRSS:\t  1234 kB\n"),
            Some(1234)
        );
    }

    #[test]
    fn parses_rss_from_a_status_fixture_with_unrelated_fields() {
        let status = "Name:\tlavis\nVmSize:\t 20480 kB\nVmRSS: 10624 kB\nThreads:\t2\n";

        assert_eq!(parse_memory_kib(status), Some(10624));
    }

    #[test]
    fn rejects_missing_or_malformed_memory_kib() {
        assert_eq!(parse_memory_kib("Name:\tlavis\n"), None);
        assert_eq!(parse_memory_kib("VmRSS: bad kB\n"), None);
        assert_eq!(parse_memory_kib("VmRSS: 1234 bytes\n"), None);
    }

    #[test]
    fn formats_stats_with_all_labels_and_values() {
        let output = format_stats(
            "12 ms",
            Duration::from_secs(61),
            &ProcStats {
                system_uptime: Some(Duration::from_secs(3_600)),
                memory_kib: Some(10_650),
            },
            2,
        );

        assert!(output.contains("📊 Lavis stats"));
        assert!(output.contains("Telegram: 12 ms"));
        assert!(output.contains("📊 Lavis stats\n\nTelegram"));
        assert!(output.contains("Lavis uptime: 1m 01s"));
        assert!(output.contains("System uptime: 1h 00m 00s"));
        assert!(output.contains("Memory: 10.4 MiB RSS"));
        assert!(output.contains("Commands: 2"));
        assert!(output.contains("Version: 0.1.0"));
    }
}
