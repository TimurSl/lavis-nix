use std::time::{Duration, Instant};

use grammers_client::{Client, tl};

use crate::{
    aliases::{Alias, AliasStore, DeleteResult},
    command::Command,
    commands::{Action, AliasRequest, dispatch},
    fastfetch::{self, FastfetchInputError, FastfetchResult},
    help::render,
    response::Response,
};

pub struct RuntimeState {
    started_at: Instant,
    recognized_commands: u64,
    aliases: AliasStore,
}

impl RuntimeState {
    pub fn new(started_at: Instant, aliases: AliasStore) -> Self {
        Self {
            started_at,
            recognized_commands: 0,
            aliases,
        }
    }

    pub fn resolve_alias(&self, name: &str, args: &str) -> Option<Action> {
        let invocation_args = shell_words::split(args).ok()?;
        let invocation = self.aliases.invocation(name, &invocation_args).ok()??;
        dispatch(&Command {
            name: invocation.target,
            args: shell_words::join(invocation.args),
        })
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
                let rendered = render(request, prefix, &self.aliases);
                if rendered.entity_fallback {
                    tracing::warn!(
                        event = "help_entity_fallback",
                        "Help formatting was unavailable"
                    );
                }
                rendered.response
            }
            Action::Fastfetch(arguments) => fastfetch_response(fastfetch::run(arguments).await),
            Action::Alias(request) => self.execute_alias(request, prefix).await,
        }
    }

    async fn execute_alias(&mut self, request: &AliasRequest, prefix: &str) -> Response {
        match request {
            AliasRequest::List => {
                let aliases = self.aliases.aliases();
                if aliases.is_empty() {
                    return Response::plain("🔗 No aliases configured");
                }
                Response::plain(format!(
                    "🔗 Aliases\n\n{}",
                    aliases
                        .iter()
                        .map(|(name, alias)| {
                            let args = if alias.args.is_empty() {
                                String::new()
                            } else {
                                format!(" {}", shell_words::join(&alias.args))
                            };
                            format!("{prefix}{name} → {prefix}{}{args}", alias.target)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            }
            AliasRequest::Add { name, target, args } => match self
                .aliases
                .add(
                    name,
                    Alias {
                        target: target.clone(),
                        args: args.clone(),
                    },
                )
                .await
            {
                Ok(_) => Response::plain(format!("🔗 Added alias: {prefix}{name}")),
                Err(error) => Response::plain(format!("⚠️ Could not add alias: {error}")),
            },
            AliasRequest::Delete { name } => match self.aliases.delete(name).await {
                Ok(DeleteResult::Deleted) => {
                    Response::plain(format!("🔗 Deleted alias: {prefix}{name}"))
                }
                Ok(DeleteResult::NotFound) => {
                    Response::plain(format!("❓ Alias not found: {name}"))
                }
                Err(error) => Response::plain(format!("⚠️ Could not delete alias: {error}")),
            },
            AliasRequest::Show { name } => {
                let normalized_name = name.to_ascii_lowercase();
                let Some(alias) = self.aliases.lookup(name) else {
                    return Response::plain(format!(
                        "⚠️ Alias {prefix}{normalized_name} does not exist"
                    ));
                };
                let args = if alias.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", shell_words::join(&alias.args))
                };
                Response::collapsed(
                    format!("🔗 {prefix}{normalized_name}"),
                    format!("Alias for:\n{prefix}{}{args}", alias.target),
                )
                .response
            }
            AliasRequest::Invalid => Response::plain(format!(
                "⚠️ Usage: {prefix}alias [list|add <name> <command> [arguments...]|show <name>|del <name>]"
            )),
        }
    }
}

fn fastfetch_response(result: FastfetchResult) -> Response {
    match result {
        FastfetchResult::Success(response) => response,
        FastfetchResult::Empty => Response::plain("⚠️ Fastfetch produced no output"),
        FastfetchResult::TimedOut => Response::plain("⚠️ Fastfetch timed out"),
        FastfetchResult::Unavailable => Response::plain("⚠️ Fastfetch is unavailable"),
        FastfetchResult::NonZero { code, .. } => {
            Response::plain(format!("⚠️ Fastfetch failed (exit code {code})"))
        }
        FastfetchResult::UnexpectedStatus => Response::plain("⚠️ Fastfetch ended unexpectedly"),
        FastfetchResult::InvalidArguments(error) => Response::plain(format!(
            "⚠️ Invalid fastfetch options: {}",
            fastfetch_input_message(error)
        )),
    }
}

fn fastfetch_input_message(error: FastfetchInputError) -> &'static str {
    match error {
        FastfetchInputError::Tokenization => "invalid quoting",
        FastfetchInputError::UnsupportedOption => "unsupported option",
        FastfetchInputError::MissingValue => "option value is missing",
        FastfetchInputError::InvalidLogo => "only --logo none is allowed",
        FastfetchInputError::InvalidStructure => "invalid --structure value",
        FastfetchInputError::InvalidSeparator => "invalid --separator value",
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
        ProcStats, fastfetch_response, format_duration, format_latency, format_stats,
        parse_memory_kib, parse_system_uptime,
    };
    use crate::response::Response;
    use crate::{
        aliases::{Alias, AliasStore},
        commands::AliasRequest,
        fastfetch::FastfetchResult,
    };
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    async fn runtime_with_alias() -> (super::RuntimeState, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("lavis-runtime-show-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("aliases.json");
        let mut aliases = AliasStore::load(path).await.unwrap();
        aliases
            .add(
                "Mini",
                Alias {
                    target: "fastfetch".to_owned(),
                    args: vec!["--separator".to_owned(), " → ".to_owned()],
                },
            )
            .await
            .unwrap();
        (super::RuntimeState::new(Instant::now(), aliases), directory)
    }

    #[tokio::test]
    async fn shows_existing_alias_with_utf16_safe_collapsed_body() {
        let (mut runtime, directory) = runtime_with_alias().await;
        let response = runtime
            .execute_alias(
                &AliasRequest::Show {
                    name: "MINI".to_owned(),
                },
                "🦀",
            )
            .await;
        let units = response.text.encode_utf16().collect::<Vec<_>>();
        let grammers_client::tl::enums::MessageEntity::Blockquote(entity) = &response.entities[0]
        else {
            panic!("expected a blockquote entity");
        };

        assert_eq!(
            response.text,
            "🔗 🦀mini\n\nAlias for:\n🦀fastfetch --separator ' → '"
        );
        assert_eq!(response.entities.len(), 1);
        assert!(entity.collapsed);
        let offset = usize::try_from(entity.offset).unwrap();
        let length = usize::try_from(entity.length).unwrap();
        assert_eq!(
            String::from_utf16(&units[..offset]).unwrap(),
            "🔗 🦀mini\n\n"
        );
        assert_eq!(
            String::from_utf16(&units[offset..offset + length]).unwrap(),
            "Alias for:\n🦀fastfetch --separator ' → '"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn reports_missing_alias_and_invalid_show_usage() {
        let (mut runtime, directory) = runtime_with_alias().await;

        assert_eq!(
            runtime
                .execute_alias(
                    &AliasRequest::Show {
                        name: "MISSING".to_owned()
                    },
                    "!"
                )
                .await,
            Response::plain("⚠️ Alias !missing does not exist")
        );
        assert_eq!(
            runtime.execute_alias(&AliasRequest::Invalid, "!").await,
            Response::plain(
                "⚠️ Usage: !alias [list|add <name> <command> [arguments...]|show <name>|del <name>]"
            )
        );
        fs::remove_dir_all(directory).unwrap();
    }

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
    fn reports_fastfetch_exit_codes_without_stderr() {
        assert_eq!(
            fastfetch_response(FastfetchResult::NonZero {
                code: 1,
                stderr: "sensitive diagnostic".to_owned(),
            })
            .text,
            "⚠️ Fastfetch failed (exit code 1)"
        );
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
