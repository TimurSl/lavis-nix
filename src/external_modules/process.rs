use super::{
    manifest::ExternalModuleDescriptor,
    protocol::{self, CoreMessage, MAX_LINE_BYTES, MAX_RESULT_BYTES, ModuleMessage},
};
use crate::error::ExternalError;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
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
    child: Option<Child>,
    pid: u32,
    stdin: tokio::process::ChildStdin,
    stdout_reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    stderr_drain: Option<tokio::task::JoinHandle<StderrCapture>>,
    descriptor: ExternalModuleDescriptor,
    status: ProcessStatus,
    in_flight_request: Option<String>,
    live_replies: std::collections::VecDeque<ModuleMessage>,
}

struct StderrCapture {
    _bytes: Vec<u8>,
    _truncated: bool,
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
            .env_clear()
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("CLICOLOR_FORCE", "0")
            .env("TERM", "dumb")
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    let ret = libc::setsid();
                    if ret == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = command.spawn().map_err(|_| ExternalError::Unavailable)?;

        let pid = child.id().ok_or(ExternalError::Unavailable)?;

        let stdin = child.stdin.take().ok_or(ExternalError::Unavailable)?;
        let stderr = child.stderr.take().ok_or(ExternalError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(ExternalError::Unavailable)?;
        let stdout_reader = BufReader::new(stdout);

        let stderr_drain = tokio::spawn(drain_stderr(stderr));

        let mut process = Self {
            child: Some(child),
            pid,
            stdin,
            stdout_reader,
            stderr_drain: Some(stderr_drain),
            descriptor,
            status: ProcessStatus::Running,
            in_flight_request: None,
            live_replies: std::collections::VecDeque::new(),
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

        let response = timeout(INIT_TIMEOUT, self.read_message())
            .await
            .map_err(|_| ExternalError::HandshakeTimeout)?;

        match response {
            Ok(ModuleMessage::Initialized {
                request_id,
                module_id,
            }) => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                if module_id != self.descriptor.id {
                    return Err(ExternalError::ProtocolVersionMismatch);
                }
                Ok(())
            }
            Ok(_) => Err(ExternalError::ProtocolDecode),
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

        let result = timeout(EXECUTE_TIMEOUT, self.collect_reply(&req_id))
            .await
            .map_err(|_| {
                self.status = ProcessStatus::Crashed;
                ExternalError::ExecutionTimeout
            });

        self.in_flight_request = None;

        match result {
            Ok(ModuleMessage::Result { request_id, text }) => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                Ok(truncate_result(&text))
            }
            Ok(ModuleMessage::Error {
                request_id,
                code: _,
                message: _,
            }) => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                Err(ExternalError::ModuleError)
            }
            Ok(_) => Err(ExternalError::ProtocolDecode),
            Err(e) => Err(e),
        }
    }

    async fn collect_reply(&mut self, expected_id: &str) -> Result<ModuleMessage, ExternalError> {
        while let Some(msg) = self.live_replies.pop_front() {
            match &msg {
                ModuleMessage::Log { .. } => continue,
                ModuleMessage::Result { request_id, .. }
                | ModuleMessage::Error { request_id, .. }
                | ModuleMessage::Health { request_id } => {
                    if request_id == expected_id {
                        return Ok(msg);
                    }
                    if request_id.as_str() < expected_id {
                        continue;
                    }
                    return Err(ExternalError::WrongRequestId);
                }
                _ => return Err(ExternalError::ProtocolDecode),
            }
        }

        loop {
            let line = self.read_line().await?;
            match line {
                Some(ModuleMessage::Log { .. }) => continue,
                Some(
                    ModuleMessage::Result { request_id, .. }
                    | ModuleMessage::Error { request_id, .. }
                    | ModuleMessage::Health { request_id },
                ) => {
                    if request_id == expected_id {
                        return Ok(line.unwrap());
                    }
                    if request_id.as_str() < expected_id {
                        continue;
                    }
                    self.live_replies.push_back(line.unwrap());
                    return Err(ExternalError::WrongRequestId);
                }
                Some(_) => return Err(ExternalError::ProtocolDecode),
                None => {
                    self.status = ProcessStatus::Crashed;
                    return Err(ExternalError::Unavailable);
                }
            }
        }
    }

    pub async fn health_check(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Health {
            request_id: req_id.clone(),
        };
        self.send(&msg).await?;

        let response = timeout(HEALTH_TIMEOUT, self.collect_reply(&req_id))
            .await
            .map_err(|_| ExternalError::ExecutionTimeout)?;

        match response {
            ModuleMessage::Health { request_id } => {
                if request_id != req_id {
                    return Err(ExternalError::WrongRequestId);
                }
                Ok(())
            }
            _ => Err(ExternalError::ProtocolDecode),
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

        match timeout(SHUTDOWN_TIMEOUT, self.reap_child()).await {
            Ok(()) => {
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
        self.terminate_process_group().await;
        self.reap_child().await;
    }

    async fn terminate_process_group(&self) {
        #[cfg(unix)]
        {
            let pgid = self.pid as i32;
            let ret = unsafe { libc::kill(-pgid, libc::SIGKILL) };
            if ret == -1 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    tracing::warn!(
                        event = "process_group_kill_failed",
                        pid = self.pid,
                        error = %err,
                        "Failed to kill process group"
                    );
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.pid;
        }
    }

    async fn reap_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.wait().await;
        }
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

    async fn read_message(&mut self) -> Result<ModuleMessage, ExternalError> {
        let line = self.read_line().await?;
        line.ok_or(ExternalError::Unavailable)
    }

    async fn read_line(&mut self) -> Result<Option<ModuleMessage>, ExternalError> {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut single = [0u8; 1];
        loop {
            let n = self
                .stdout_reader
                .read(&mut single)
                .await
                .map_err(|_| ExternalError::ProtocolDecode)?;
            if n == 0 {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            if single[0] == b'\n' {
                break;
            }
            if buf.len() >= MAX_LINE_BYTES {
                return Err(ExternalError::LineTooLarge);
            }
            buf.push(single[0]);
        }

        let trimmed = std::str::from_utf8(&buf).map_err(|_| ExternalError::ProtocolDecode)?;

        protocol::parse_module_line(trimmed)
    }
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr) -> StderrCapture {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(MAX_STDERR_CAPTURE);
    let mut truncated = false;
    let mut tmp = [0u8; 1024];
    loop {
        let n = match stderr.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if truncated {
            continue;
        }
        let remaining = MAX_STDERR_CAPTURE.saturating_sub(buf.len());
        if n <= remaining {
            buf.extend_from_slice(&tmp[..n]);
        } else {
            buf.extend_from_slice(&tmp[..remaining]);
            truncated = true;
        }
    }
    StderrCapture {
        _bytes: buf,
        _truncated: truncated,
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
        format!("{}…", &text[..end])
    }
}

pub async fn reap_child(mut child: Child) {
    let _ = child.wait().await;
}

#[cfg(all(test, feature = "fixture-tests"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn create_echo_module() -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-proc-test-{nonce}"));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();

        let fixture_path = dir.join("bin").join("echo-module");
        compile_fixture(
            &fixture_path,
            r#"
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let val: serde_json::Value = serde_json::from_str(&line)?;
        let req_id = val["request_id"].as_str().unwrap_or("?").to_owned();
        match val["type"].as_str().unwrap_or("") {
            "initialize" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "initialized",
                    "request_id": req_id,
                    "module_id": val["module_id"],
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
            }
            "execute" => {
                let args = val["arguments"].as_str().unwrap_or("");
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "result",
                    "request_id": req_id,
                    "text": args,
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
            }
            "health" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "health",
                    "request_id": req_id,
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
            }
            "shutdown" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "health",
                    "request_id": req_id,
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Ok(())
}
"#,
        );

        let _ = std::process::Command::new("chmod")
            .args(["+x", &fixture_path.to_string_lossy()])
            .output();

        let descriptor = ExternalModuleDescriptor {
            id: "echo".to_owned(),
            display_name: "Echo".to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            capabilities: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    fn create_child_spawner_module() -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-proc-child-{nonce}"));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();

        let fixture_path = dir.join("bin").join("child-spawner");
        compile_fixture(
            &fixture_path,
            r#"
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{self, BufRead, Write};
    use std::process::{Command, Stdio};
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let val: serde_json::Value = serde_json::from_str(&line)?;
        let req_id = val["request_id"].as_str().unwrap_or("?").to_owned();
        match val["type"].as_str().unwrap_or("") {
            "initialize" => {
                let _child = Command::new("sh")
                    .arg("-c")
                    .arg("sleep 60")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "initialized",
                    "request_id": req_id,
                    "module_id": val["module_id"],
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
            }
            "execute" => {
                let args = val["arguments"].as_str().unwrap_or("");
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "result",
                    "request_id": req_id,
                    "text": args,
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
            }
            "health" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "health",
                    "request_id": req_id,
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
            }
            "shutdown" => {
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "health",
                    "request_id": req_id,
                });
                writeln!(stdout, "{}", resp)?;
                stdout.flush()?;
                std::process::exit(0);
            }
            _ => {}
        }
    }
}
"#,
        );

        let _ = std::process::Command::new("chmod")
            .args(["+x", &fixture_path.to_string_lossy()])
            .output();

        let descriptor = ExternalModuleDescriptor {
            id: "child-spawner".to_owned(),
            display_name: "ChildSpawner".to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            capabilities: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    fn compile_fixture(output: &Path, source: &str) {
        use std::process::Command;
        let rustc = Command::new("rustc")
            .args(["-", "-o", &output.to_string_lossy(), "--edition", "2021"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("rustc must be available for tests");
        if let Some(mut stdin) = rustc.stdin {
            use std::io::Write;
            let _ = stdin.write_all(source.as_bytes());
        }
        let _ = rustc.wait_with_output();
    }

    fn create_fixture_module(source: &str, id: &str) -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-fixture-{nonce}"));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();

        let fixture_path = dir.join("bin").join(id);
        compile_fixture(&fixture_path, source);

        let _ = std::process::Command::new("chmod")
            .args(["+x", &fixture_path.to_string_lossy()])
            .output();

        let descriptor = ExternalModuleDescriptor {
            id: id.to_owned(),
            display_name: id.to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
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
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_execute_command() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        let result = proc.execute("repeat", "Привет 🎉").await.unwrap();
        assert_eq!(result, "Привет 🎉");
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_health_check() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        proc.health_check().await.unwrap();
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        proc.graceful_shutdown().await.unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_wrong_protocol_version() {
        let source = r#"
fn main() {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let val: serde_json::Value = serde_json::from_str(&line).unwrap();
        let req_id = val["request_id"].as_str().unwrap_or("?").to_owned();
        match val["type"].as_str().unwrap_or("") {
            "initialize" => {
                let resp = serde_json::json!({
                    "protocol_version": 1,
                    "type": "initialized",
                    "request_id": req_id,
                    "module_id": val["module_id"],
                });
                writeln!(stdout, "{}", resp).unwrap();
                stdout.flush().unwrap();
            }
            _ => {}
        }
    }
}
"#;
        let (desc, dir) = create_fixture_module(source, "bad-proto");
        let result = ModuleProcess::start(desc, &dir).await;
        assert!(matches!(
            result,
            Err(ExternalError::ProtocolVersionMismatch)
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_execute_timeout() {
        let source = r#"
fn main() {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let val: serde_json::Value = serde_json::from_str(&line).unwrap();
        let req_id = val["request_id"].as_str().unwrap_or("?").to_owned();
        match val["type"].as_str().unwrap_or("") {
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
                std::thread::sleep(std::time::Duration::from_secs(10));
                let resp = serde_json::json!({
                    "protocol_version": 2,
                    "type": "result",
                    "request_id": req_id,
                    "text": "too late",
                });
                writeln!(stdout, "{}", resp).unwrap();
                stdout.flush().unwrap();
            }
            _ => {}
        }
    }
}
"#;
        let (desc, dir) = create_fixture_module(source, "timeout");
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        let result = proc.execute("repeat", "test").await;
        assert!(matches!(result, Err(ExternalError::ExecutionTimeout)));
        assert_eq!(proc.status(), ProcessStatus::Failed);
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_terminate_process_group() {
        let (desc, dir) = create_child_spawner_module();
        let mut proc = ModuleProcess::start(desc, &dir).await.unwrap();
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }
}
