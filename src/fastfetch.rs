use std::{io, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    task::JoinHandle,
    time::timeout,
};

use crate::response::{Response, TRUNCATION_SUFFIX};

const STDOUT_CAP: usize = 64 * 1024;
const STDERR_CAP: usize = 16 * 1024;
const STDERR_EXCERPT_UNITS: usize = 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastfetchInputError {
    Tokenization,
    UnsupportedOption,
    MissingValue,
    InvalidLogo,
    InvalidStructure,
    InvalidSeparator,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FastfetchResult {
    Success(Response),
    Empty,
    TimedOut,
    Unavailable,
    NonZero { code: i32, stderr: String },
    UnexpectedStatus,
    InvalidArguments(FastfetchInputError),
}

struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

pub async fn run(arguments: &str) -> FastfetchResult {
    let tokens = match tokenize(arguments) {
        Ok(tokens) => tokens,
        Err(error) => return FastfetchResult::InvalidArguments(error),
    };
    let options = match validate_options(&tokens) {
        Ok(options) => options,
        Err(error) => return FastfetchResult::InvalidArguments(error),
    };
    run_options(&options).await
}

pub fn tokenize(arguments: &str) -> Result<Vec<String>, FastfetchInputError> {
    shell_words::split(arguments).map_err(|_| FastfetchInputError::Tokenization)
}

pub fn validate_options(tokens: &[String]) -> Result<Vec<String>, FastfetchInputError> {
    let mut options = Vec::new();
    let mut tokens = tokens.iter();
    while let Some(option) = tokens.next() {
        let value = tokens.next().ok_or(FastfetchInputError::MissingValue)?;
        match option.as_str() {
            "--logo" => {
                if value != "none" {
                    return Err(FastfetchInputError::InvalidLogo);
                }
                options.extend([option.clone(), value.clone()]);
            }
            "--structure" => {
                let structure = validate_structure(value)?;
                options.extend([option.clone(), structure]);
            }
            "--separator" => {
                if !(1..=64).contains(&value.chars().count())
                    || value.starts_with("--")
                    || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
                {
                    return Err(FastfetchInputError::InvalidSeparator);
                }
                options.push(format!("--separator={value}"));
            }
            _ => return Err(FastfetchInputError::UnsupportedOption),
        }
    }
    Ok(options)
}

fn validate_structure(value: &str) -> Result<String, FastfetchInputError> {
    let mut components = Vec::new();
    for component in value.split(':') {
        let canonical = match component.to_ascii_lowercase().as_str() {
            "os" => "OS",
            "kernel" => "Kernel",
            "cpu" => "CPU",
            "gpu" => "GPU",
            "memory" => "Memory",
            _ => return Err(FastfetchInputError::InvalidStructure),
        };
        components.push(canonical);
    }
    if components.is_empty() {
        return Err(FastfetchInputError::InvalidStructure);
    }
    Ok(components.join(":"))
}

async fn run_options(options: &[String]) -> FastfetchResult {
    let mut command = Command::new("fastfetch");
    command
        .args(["--config", "none", "--pipe"])
        .args(options)
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .env_remove("LAVIS_API_ID")
        .env_remove("LAVIS_API_HASH")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "0")
        .env("TERM", "dumb");

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return FastfetchResult::Unavailable,
    };
    let Some(stdout) = child.stdout.take() else {
        if !terminate_and_wait(&mut child).await {
            tracing::debug!(
                event = "fastfetch_cleanup_failed",
                "Fastfetch cleanup failed"
            );
        }
        return FastfetchResult::UnexpectedStatus;
    };
    let Some(stderr) = child.stderr.take() else {
        if !terminate_and_wait(&mut child).await {
            tracing::debug!(
                event = "fastfetch_cleanup_failed",
                "Fastfetch cleanup failed"
            );
        }
        return FastfetchResult::UnexpectedStatus;
    };
    let mut stdout_task = tokio::spawn(drain(stdout, STDOUT_CAP));
    let mut stderr_task = tokio::spawn(drain(stderr, STDERR_CAP));

    let status = match timeout(TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            let terminated = terminate_and_wait(&mut child).await;
            let drains = await_drains(&mut stdout_task, &mut stderr_task).await;
            if !terminated || drains.is_none() {
                tracing::debug!(
                    event = "fastfetch_cleanup_failed",
                    "Fastfetch cleanup failed"
                );
            }
            return FastfetchResult::UnexpectedStatus;
        }
        Err(_) => {
            let terminated = terminate_and_wait(&mut child).await;
            let drains = await_drains_with_grace(&mut stdout_task, &mut stderr_task).await;
            if !terminated || drains.is_none() {
                tracing::debug!(
                    event = "fastfetch_cleanup_failed",
                    "Fastfetch cleanup failed"
                );
            }
            return FastfetchResult::TimedOut;
        }
    };
    let (stdout, stderr) = match await_drains(&mut stdout_task, &mut stderr_task).await {
        Some(captures) => captures,
        None => return FastfetchResult::UnexpectedStatus,
    };

    if status.success() {
        let output = sanitize_capture(&stdout);
        if output.is_empty() {
            FastfetchResult::Empty
        } else {
            FastfetchResult::Success(Response::preformatted(output))
        }
    } else if let Some(code) = status.code() {
        FastfetchResult::NonZero {
            code,
            stderr: truncate_excerpt(&sanitize_capture(&stderr)),
        }
    } else {
        FastfetchResult::UnexpectedStatus
    }
}

async fn terminate_and_wait(child: &mut Child) -> bool {
    let kill_started = child.start_kill().is_ok();
    let waited = child.wait().await.is_ok();
    kill_started && waited
}

async fn await_drains(
    stdout_task: &mut JoinHandle<io::Result<Capture>>,
    stderr_task: &mut JoinHandle<io::Result<Capture>>,
) -> Option<(Capture, Capture)> {
    let stdout = stdout_task.await;
    let stderr = stderr_task.await;
    match (stdout, stderr) {
        (Ok(Ok(stdout)), Ok(Ok(stderr))) => Some((stdout, stderr)),
        _ => None,
    }
}

async fn await_drains_with_grace(
    stdout_task: &mut JoinHandle<io::Result<Capture>>,
    stderr_task: &mut JoinHandle<io::Result<Capture>>,
) -> Option<(Capture, Capture)> {
    match timeout(DRAIN_GRACE, await_drains(stdout_task, stderr_task)).await {
        Ok(captures) => captures,
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            let stdout = stdout_task.await;
            let stderr = stderr_task.await;
            if stdout.is_err() || stderr.is_err() {
                tracing::debug!(
                    event = "fastfetch_drain_abort_failed",
                    "Fastfetch drain abort failed"
                );
            }
            None
        }
    }
}

async fn drain<R>(mut reader: R, cap: usize) -> io::Result<Capture>
where
    R: AsyncRead + Unpin,
{
    let mut capture = Capture {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(capture);
        }
        append_capture(&mut capture, &buffer[..count], cap);
    }
}

fn append_capture(capture: &mut Capture, chunk: &[u8], cap: usize) {
    let available = cap.saturating_sub(capture.bytes.len());
    let captured = available.min(chunk.len());
    capture.bytes.extend_from_slice(&chunk[..captured]);
    capture.truncated |= captured < chunk.len();
}

fn sanitize_capture(capture: &Capture) -> String {
    let stripped = strip_ansi_escapes::strip(normalize_input_bytes(&capture.bytes));
    let normalized = String::from_utf8_lossy(&stripped);
    let mut output = String::new();
    for character in normalized.chars() {
        match character {
            '\n' => output.push('\n'),
            '\t' => output.push_str("    "),
            character
                if character == '\0' || character.is_control() || is_bidi_control(character) => {}
            character => output.push(character),
        }
    }
    let output = output
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_owned();
    if capture.truncated {
        if output.is_empty() {
            TRUNCATION_SUFFIX.to_owned()
        } else {
            format!("{output}\n{TRUNCATION_SUFFIX}")
        }
    } else {
        output
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn normalize_input_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                normalized.push(b'\n');
                index += usize::from(bytes.get(index + 1) == Some(&b'\n'));
            }
            b'\t' => normalized.extend_from_slice(b"    "),
            byte => normalized.push(byte),
        }
        index += 1;
    }
    normalized
}

fn truncate_excerpt(text: &str) -> String {
    if text.encode_utf16().count() <= STDERR_EXCERPT_UNITS {
        return text.to_owned();
    }
    let suffix_units = TRUNCATION_SUFFIX.encode_utf16().count();
    let limit = STDERR_EXCERPT_UNITS.saturating_sub(suffix_units);
    let mut end = 0;
    let mut units = 0usize;
    for (index, character) in text.char_indices() {
        if units.saturating_add(character.len_utf16()) > limit {
            break;
        }
        units += character.len_utf16();
        end = index + character.len_utf8();
    }
    format!("{}{}", &text[..end], TRUNCATION_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::{
        Capture, FastfetchInputError, append_capture, sanitize_capture, tokenize, validate_options,
    };

    #[test]
    fn tokenizes_quotes_escapes_and_shell_syntax_literally() {
        assert_eq!(
            tokenize("--logo none --structure OS:Kernel:CPU:GPU:Memory --separator \" -> \"")
                .unwrap(),
            [
                "--logo",
                "none",
                "--structure",
                "OS:Kernel:CPU:GPU:Memory",
                "--separator",
                " -> "
            ]
        );
        assert_eq!(
            tokenize("'a b' escaped\\ space ; $() `x`").unwrap(),
            ["a b", "escaped space", ";", "$()", "`x`"]
        );
        assert_eq!(
            tokenize("'unterminated"),
            Err(FastfetchInputError::Tokenization)
        );
    }

    #[test]
    fn validates_only_safe_fastfetch_options() {
        assert_eq!(
            validate_options(&["--structure".to_owned(), "os:KERNEL:cpu".to_owned()]).unwrap(),
            ["--structure", "OS:Kernel:CPU"]
        );
        assert_eq!(
            validate_options(&["--separator".to_owned(), " -> ".to_owned()]).unwrap(),
            ["--separator= -> "]
        );
        assert_eq!(
            validate_options(&["--logo".to_owned(), "small".to_owned()]),
            Err(FastfetchInputError::InvalidLogo)
        );
        assert_eq!(
            validate_options(&["--config".to_owned(), "file".to_owned()]),
            Err(FastfetchInputError::UnsupportedOption)
        );
        assert_eq!(
            validate_options(&["--separator".to_owned(), "bad\n".to_owned()]),
            Err(FastfetchInputError::InvalidSeparator)
        );
        assert_eq!(
            validate_options(&["--separator".to_owned(), "--unsafe".to_owned()]),
            Err(FastfetchInputError::InvalidSeparator)
        );
        assert_eq!(
            validate_options(&["--separator".to_owned(), "→".to_owned()]),
            Err(FastfetchInputError::InvalidSeparator)
        );
    }

    #[test]
    fn sanitizes_ansi_controls_carriage_returns_and_invalid_utf8() {
        let capture = Capture {
            bytes: b"\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b[2J\r\nline\rnext\0\x01\t  \xff\xe2\x80\xaehidden"
                .to_vec(),
            truncated: false,
        };

        assert_eq!(sanitize_capture(&capture), "red\nline\nnext      �hidden");
    }

    #[test]
    fn bounded_capture_keeps_draining_after_its_cap() {
        let mut capture = Capture {
            bytes: Vec::new(),
            truncated: false,
        };
        append_capture(&mut capture, b"abcd", 3);
        append_capture(&mut capture, b"efgh", 3);

        assert_eq!(capture.bytes, b"abc");
        assert!(capture.truncated);
    }
}
