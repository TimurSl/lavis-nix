use super::{
    manifest::ExternalModuleDescriptor,
    protocol::{self, CoreMessage, MAX_LINE_BYTES, MAX_RESULT_BYTES, ModuleMessage},
};
use crate::error::ExternalError;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    time::timeout,
};

pub const INIT_TIMEOUT: Duration = Duration::from_secs(2);
pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(5);
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_STDERR_CAPTURE: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Failed,
    Crashed,
    Terminated,
}

pub struct ModuleProcess {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    descriptor: ExternalModuleDescriptor,
    status: ProcessStatus,
    in_flight_request: Option<String>,
}

impl ModuleProcess {
    pub fn descriptor(&self) -> &ExternalModuleDescriptor {
        &self.descriptor
    }

    pub fn status(&self) -> ProcessStatus {
        self.status
    }

    pub fn id(&self) -> &str {
        &self.descriptor.id
    }

    pub fn in_flight_request(&self) -> Option<&str> {
        self.in_flight_request.as_deref()
    }

    pub async fn start(
        descriptor: ExternalModuleDescriptor,
        module_root: &Path,
    ) -> Result<Self, ExternalError> {
        let entrypoint = descriptor.entrypoint.clone();
        if !entrypoint.starts_with(module_root) {
            return Err(ExternalError::PathEscape);
        }

        let mut command = Command::new(&entrypoint);
        command
            .current_dir(module_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("LAVIS_API_ID")
            .env_remove("LAVIS_API_HASH")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("CLICOLOR_FORCE", "0")
            .env("TERM", "dumb")
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            // use std::os::unix::process::CommandExt;
            // process_group not needed; setsid() for process group
            unsafe {
                command.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let mut child = command.spawn().map_err(|_| ExternalError::Unavailable)?;

        let stdin = child.stdin.take().ok_or(ExternalError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(ExternalError::Unavailable)?;
        let stdout_reader = BufReader::new(stdout);

        let mut process = Self {
            child,
            stdin,
            stdout_reader,
            descriptor,
            status: ProcessStatus::Running,
            in_flight_request: None,
        };

        process.handshake().await?;
        Ok(process)
    }

    async fn handshake(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Initialize {
            request_id: req_id.clone(),
            module_id: self.descriptor.id.clone(),
        };
        self.send(&msg).await?;

        let response = timeout(INIT_TIMEOUT, self.read_line())
            .await
            .map_err(|_| ExternalError::HandshakeTimeout)?;

        match response {
            Ok(Some(ModuleMessage::Initialized {
                request_id,
                module_id,
            })) => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                if module_id != self.descriptor.id {
                    return Err(ExternalError::ProtocolVersionMismatch);
                }
                Ok(())
            }
            Ok(Some(_)) => Err(ExternalError::ProtocolDecode),
            Ok(None) => Err(ExternalError::Unavailable),
            Err(e) => Err(e),
        }
    }

    pub async fn execute(
        &mut self,
        command: &str,
        arguments: &str,
    ) -> Result<String, ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Execute {
            request_id: req_id.clone(),
            command: command.to_owned(),
            arguments: arguments.to_owned(),
        };
        self.in_flight_request = Some(req_id.clone());
        self.send(&msg).await?;

        let result = timeout(EXECUTE_TIMEOUT, self.read_line())
            .await
            .map_err(|_| ExternalError::ExecutionTimeout);

        self.in_flight_request = None;

        match result {
            Ok(Ok(Some(ModuleMessage::Result { request_id, text }))) => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                Ok(truncate_result(&text))
            }
            Ok(Ok(Some(ModuleMessage::Error {
                request_id,
                code: _,
                message: _,
            }))) => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                Err(ExternalError::ModuleError)
            }
            Ok(Ok(Some(_))) => Err(ExternalError::ProtocolDecode),
            Ok(Ok(None)) => {
                self.status = ProcessStatus::Crashed;
                Err(ExternalError::Unavailable)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.status = ProcessStatus::Crashed;
                Err(ExternalError::ExecutionTimeout)
            }
        }
    }

    pub async fn health_check(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Health {
            request_id: req_id.clone(),
        };
        self.send(&msg).await?;

        let response = timeout(HEALTH_TIMEOUT, self.read_line())
            .await
            .map_err(|_| ExternalError::ExecutionTimeout)?;

        match response {
            Ok(Some(ModuleMessage::Health { request_id })) => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                Ok(())
            }
            Ok(Some(_)) => Err(ExternalError::ProtocolDecode),
            Ok(None) => {
                self.status = ProcessStatus::Crashed;
                Err(ExternalError::Unavailable)
            }
            Err(e) => Err(e),
        }
    }

    pub async fn graceful_shutdown(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Shutdown {
            request_id: req_id.clone(),
        };
        if self.send(&msg).await.is_err() {
            self.terminate().await;
            return Err(ExternalError::ShutdownTimeout);
        }

        match timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await {
            Ok(_) => {
                self.status = ProcessStatus::Terminated;
                Ok(())
            }
            Err(_) => {
                self.terminate().await;
                Err(ExternalError::ShutdownTimeout)
            }
        }
    }

    pub fn mark_failed(&mut self) {
        self.status = ProcessStatus::Failed;
    }

    pub async fn terminate(&mut self) {
        self.status = ProcessStatus::Terminated;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    async fn send(&mut self, msg: &CoreMessage) -> Result<(), ExternalError> {
        let line = msg.serialize()?;
        let mut full = line;
        full.push('\n');
        self.stdin
            .write_all(full.as_bytes())
            .await
            .map_err(|_| ExternalError::ProtocolEncode)?;
        self.stdin
            .flush()
            .await
            .map_err(|_| ExternalError::ProtocolEncode)?;
        Ok(())
    }

    async fn read_line(&mut self) -> Result<Option<ModuleMessage>, ExternalError> {
        let mut line = String::new();
        let bytes_read = self
            .stdout_reader
            .read_line(&mut line)
            .await
            .map_err(|_| ExternalError::ProtocolDecode)?;

        if bytes_read == 0 {
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.len() > MAX_LINE_BYTES {
            return Err(ExternalError::LineTooLarge);
        }

        protocol::parse_module_line(trimmed)
    }
}

fn truncate_result(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        text.to_owned()
    } else {
        let mut end = 0;
        for (count, (idx, _)) in text.char_indices().enumerate() {
            if count >= MAX_RESULT_BYTES {
                break;
            }
            end = idx;
        }
        format!("{text}…", text = &text[..end])
    }
}

pub async fn reap_child(mut child: Child) {
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_echo_module() -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-proc-test-{nonce}"));
        fs::create_dir_all(dir.join("bin")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();
        }

        // Create a small Rust echo fixture that implements Module API v2
        let fixture_path = dir.join("bin").join("echo-module");
        let fixture_src = r##"use std::io::{self, BufRead, Write};
fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let val: serde_json::Value = serde_json::from_str(&line).unwrap();
        let req_id = val["request_id"].as_str().unwrap();
        match val["type"].as_str().unwrap() {
            "initialize" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "initialized",
                    "request_id": req_id,
                    "module_id": val["module_id"],
                });
                writeln!(stdout, "{}", resp).unwrap();
                stdout.flush().unwrap();
            }
            "execute" => {
                let args = val["arguments"].as_str().unwrap_or("");
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "result",
                    "request_id": req_id,
                    "text": args,
                });
                writeln!(stdout, "{}", resp).unwrap();
                stdout.flush().unwrap();
            }
            "health" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "health",
                    "request_id": req_id,
                });
                writeln!(stdout, "{}", resp).unwrap();
                stdout.flush().unwrap();
            }
            "shutdown" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "health",
                    "request_id": req_id,
                });
                writeln!(stdout, "{}", resp).unwrap();
                stdout.flush().unwrap();
                std::process::exit(0);
            }
            _ => {}
        }
    }
}
"##;
        fs::write(&fixture_path, fixture_src).unwrap();
        // For the test, we'll use a simple script instead of compiling Rust
        let script_path = dir.join("bin").join("echo-module.sh");
        fs::write(&script_path, "#!/bin/sh\nwhile IFS= read -r line; do\n  if echo \"$line\" | grep -q '\"type\":\"initialize\"'; then\n    echo '{\"protocol_version\":2,\"type\":\"initialized\",\"request_id\":\"'$(echo \"$line\" | sed 's/.*\"request_id\":\"\\([^\"]*\\).*/\\1/')'\",\"module_id\":\"echo\"}'\n  elif echo \"$line\" | grep -q '\"type\":\"execute\"'; then\n    ARGS=$(echo \"$line\" | sed 's/.*\"arguments\":\"\\([^\"]*\\).*/\\1/')\n    RID=$(echo \"$line\" | sed 's/.*\"request_id\":\"\\([^\"]*\\).*/\\1/')\n    echo '{\"protocol_version\":2,\"type\":\"result\",\"request_id\":\"'$RID'\",\"text\":\"'$ARGS'\"}'\n  elif echo \"$line\" | grep -q '\"type\":\"health\"'; then\n    RID=$(echo \"$line\" | sed 's/.*\"request_id\":\"\\([^\"]*\\).*/\\1/')\n    echo '{\"protocol_version\":2,\"type\":\"health\",\"request_id\":\"'$RID'\"}'\n  elif echo \"$line\" | grep -q '\"type\":\"shutdown\"'; then\n    exit 0\n  fi\ndone\n").unwrap();
        let _ = std::process::Command::new("chmod")
            .args(["+x", &script_path.to_string_lossy()])
            .output();

        let descriptor = ExternalModuleDescriptor {
            id: "echo".to_owned(),
            display_name: "Echo".to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: script_path,
            capabilities: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    #[tokio::test]
    async fn test_handshake_success() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        assert_eq!(proc.status(), ProcessStatus::Running);
        let _ = proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_execute_command() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        let result = proc.execute("repeat", "Привет").await.unwrap();
        assert_eq!(result, "Привет");
        let _ = proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_health_check() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        proc.health_check().await.unwrap();
        let _ = proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        proc.graceful_shutdown().await.unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
