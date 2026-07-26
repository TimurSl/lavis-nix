use super::{
    manifest::ExternalModuleDescriptor,
    protocol::{self, CoreMessage, MAX_LINE_BYTES, MAX_RESULT_BYTES, ModuleMessage},
};
use crate::error::ExternalError;
use std::{process::Stdio, time::Duration};
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

    pub async fn start(descriptor: ExternalModuleDescriptor) -> Result<Self, ExternalError> {
        let entrypoint = descriptor.entrypoint.clone();
        if !entrypoint.starts_with(&descriptor.module_dir) {
            return Err(ExternalError::PathEscape);
        }

        let mut command = Command::new(&entrypoint);
        command
            .current_dir(&descriptor.module_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            // Do not inherit the host search path (or any credentials it may
            // expose through wrappers). Entrypoints must be executable paths.
            .env("PATH", "/usr/bin:/bin")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("CLICOLOR_FORCE", "0")
            .env("TERM", "dumb")
            .kill_on_drop(true);

        #[cfg(unix)]
        {
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

        let Some(pid) = child.id() else {
            cleanup_spawned_child(&mut child, None).await;
            return Err(ExternalError::Unavailable);
        };

        let Some(stdin) = child.stdin.take() else {
            cleanup_spawned_child(&mut child, Some(pid)).await;
            return Err(ExternalError::Unavailable);
        };
        let Some(stderr) = child.stderr.take() else {
            cleanup_spawned_child(&mut child, Some(pid)).await;
            return Err(ExternalError::Unavailable);
        };
        let Some(stdout) = child.stdout.take() else {
            cleanup_spawned_child(&mut child, Some(pid)).await;
            return Err(ExternalError::Unavailable);
        };
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
        };

        if let Err(e) = process.handshake().await {
            process.terminate_process_group().await;
            process.reap_child().await;
            process.join_stderr_drain().await;
            return Err(e);
        }

        Ok(process)
    }

    async fn handshake(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Initialize {
            request_id: req_id.clone(),
            module_id: self.descriptor.id.clone(),
        };
        if let Err(e) = self.send(&msg).await {
            return Err(self.fail_and_terminate(e).await);
        }

        let response = match timeout(INIT_TIMEOUT, self.read_message()).await {
            Ok(inner) => match inner {
                Ok(msg) => msg,
                Err(e) => return Err(self.fail_and_terminate(e).await),
            },
            Err(_) => {
                return Err(self
                    .fail_and_terminate(ExternalError::HandshakeTimeout)
                    .await);
            }
        };

        match response {
            ModuleMessage::Initialized {
                request_id,
                module_id,
            } => {
                if request_id != req_id {
                    return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                }
                if module_id != self.descriptor.id {
                    return Err(self.fail_and_terminate(ExternalError::WrongModuleId).await);
                }
                Ok(())
            }
            ModuleMessage::Error { .. } => {
                Err(self.fail_and_terminate(ExternalError::ModuleError).await)
            }
            _ => Err(self.fail_and_terminate(ExternalError::ProtocolDecode).await),
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
        if let Err(e) = self.send(&msg).await {
            return Err(self.fail_and_terminate(e).await);
        }

        let result = match timeout(EXECUTE_TIMEOUT, self.collect_reply(&req_id)).await {
            Ok(inner) => match inner {
                Ok(msg) => msg,
                Err(e) => return Err(self.fail_and_terminate(e).await),
            },
            Err(_) => {
                return Err(self
                    .fail_and_terminate(ExternalError::ExecutionTimeout)
                    .await);
            }
        };

        self.in_flight_request = None;

        match result {
            ModuleMessage::Result { request_id, text } => {
                if request_id != req_id {
                    return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                }
                Ok(truncate_result(&text))
            }
            ModuleMessage::Error {
                request_id,
                code: _,
                message: _,
            } => {
                if request_id != req_id {
                    return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                }
                Err(self.fail_and_terminate(ExternalError::ModuleError).await)
            }
            _ => Err(self.fail_and_terminate(ExternalError::ProtocolDecode).await),
        }
    }

    async fn collect_reply(&mut self, expected_id: &str) -> Result<ModuleMessage, ExternalError> {
        loop {
            let line = self.read_line().await?;
            let Some(msg) = line else {
                self.status = ProcessStatus::Crashed;
                return Err(ExternalError::Unavailable);
            };
            match msg {
                ModuleMessage::Log { .. } => continue,
                ModuleMessage::Result { ref request_id, .. }
                | ModuleMessage::Error { ref request_id, .. }
                | ModuleMessage::Health { ref request_id } => {
                    if *request_id == *expected_id {
                        return Ok(msg);
                    }
                    return Err(ExternalError::WrongRequestId);
                }
                _ => return Err(ExternalError::ProtocolDecode),
            }
        }
    }

    pub async fn health_check(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Health {
            request_id: req_id.clone(),
        };
        if let Err(e) = self.send(&msg).await {
            return Err(self.fail_and_terminate(e).await);
        }

        let response = match timeout(HEALTH_TIMEOUT, self.collect_reply(&req_id)).await {
            Ok(inner) => match inner {
                Ok(msg) => msg,
                Err(e) => return Err(self.fail_and_terminate(e).await),
            },
            Err(_) => {
                return Err(self.fail_and_terminate(ExternalError::HealthTimeout).await);
            }
        };

        match response {
            ModuleMessage::Health { request_id } => {
                if request_id != req_id {
                    return Err(self.fail_and_terminate(ExternalError::WrongRequestId).await);
                }
                Ok(())
            }
            _ => Err(self.fail_and_terminate(ExternalError::ProtocolDecode).await),
        }
    }

    pub async fn graceful_shutdown(&mut self) -> Result<(), ExternalError> {
        let req_id = protocol::request_id();
        let msg = CoreMessage::Shutdown {
            request_id: req_id.clone(),
        };
        if let Err(error) = self.send(&msg).await {
            return Err(self.fail_and_terminate(error).await);
        }

        match timeout(SHUTDOWN_TIMEOUT, self.reap_child()).await {
            Ok(()) => {
                self.join_stderr_drain().await;
                self.in_flight_request = None;
                self.status = ProcessStatus::Terminated;
                Ok(())
            }
            Err(_) => Err(self
                .fail_and_terminate(ExternalError::ShutdownTimeout)
                .await),
        }
    }

    pub fn mark_failed(&mut self) {
        self.status = ProcessStatus::Failed;
    }

    async fn fail_and_terminate(&mut self, error: ExternalError) -> ExternalError {
        self.in_flight_request = None;
        self.status = ProcessStatus::Crashed;
        self.terminate_process_group().await;
        self.reap_child().await;
        self.join_stderr_drain().await;
        error
    }

    pub async fn terminate(&mut self) {
        self.status = ProcessStatus::Terminated;
        self.terminate_process_group().await;
        self.reap_child().await;
        self.join_stderr_drain().await;
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

    async fn join_stderr_drain(&mut self) {
        if let Some(handle) = self.stderr_drain.take() {
            let _ = handle.await;
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

async fn cleanup_spawned_child(child: &mut Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let ret = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        if ret == -1 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            let _ = child.kill().await;
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.wait().await;
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
    use std::env;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const ECHO_MODULE_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        args = val.get("arguments", "")
        resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": args}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "health":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "shutdown":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
        break
"#;

    const CHILD_SPAWNER_PY: &str = r#"#!/usr/bin/env python3
import sys, json, subprocess, os
child = None
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        child = subprocess.Popen(["sh", "-c", "sleep 60"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        args = val.get("arguments", "")
        resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": args}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "health":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "shutdown":
        resp = {"protocol_version": 2, "type": "health", "request_id": req_id}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
        break
if child:
    child.kill()
"#;

    fn make_script(output: &Path, body: &str) {
        let python = python_executable();
        let body = body.replacen(
            "#!/usr/bin/env python3",
            &format!("#!{}", python.display()),
            1,
        );
        fs::write(output, body).unwrap();
        fs::set_permissions(output, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn python_executable() -> PathBuf {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("python3"))
            .find(|candidate| candidate.is_file())
            .expect("fixture tests require python3 in PATH")
    }

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
        make_script(&fixture_path, ECHO_MODULE_PY);

        let descriptor = ExternalModuleDescriptor {
            id: "echo".to_owned(),
            display_name: "Echo".to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            module_dir: dir.clone(),
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
        make_script(&fixture_path, CHILD_SPAWNER_PY);

        let descriptor = ExternalModuleDescriptor {
            id: "child-spawner".to_owned(),
            display_name: "ChildSpawner".to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            module_dir: dir.clone(),
            capabilities: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    fn create_fixture_module(body: &str, id: &str) -> (ExternalModuleDescriptor, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("lavis-fixture-{nonce}"));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o700)).unwrap();

        let fixture_path = dir.join("bin").join(id);
        make_script(&fixture_path, body);

        let descriptor = ExternalModuleDescriptor {
            id: id.to_owned(),
            display_name: id.to_owned(),
            version: "0.1.0".to_owned(),
            author: "Test".to_owned(),
            entrypoint: fixture_path,
            module_dir: dir.clone(),
            capabilities: Vec::new(),
            commands: vec![],
        };
        (descriptor, dir)
    }

    #[tokio::test]
    async fn test_handshake_success() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        assert_eq!(proc.status(), ProcessStatus::Running);
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_execute_command() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("repeat", "Привет 🎉").await.unwrap();
        assert_eq!(result, "Привет 🎉");
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_health_check() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        proc.health_check().await.unwrap();
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let (desc, dir) = create_echo_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        proc.graceful_shutdown().await.unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    const BAD_PROTO_PY: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 1, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn test_wrong_protocol_version() {
        let (desc, dir) = create_fixture_module(BAD_PROTO_PY, "bad-proto");
        let result = ModuleProcess::start(desc).await;
        assert!(matches!(
            result,
            Err(ExternalError::ProtocolVersionMismatch)
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    const TIMEOUT_PY: &str = r#"#!/usr/bin/env python3
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    val = json.loads(line)
    req_id = val.get("request_id", "?")
    msg_type = val.get("type", "")
    if msg_type == "initialize":
        resp = {"protocol_version": 2, "type": "initialized", "request_id": req_id, "module_id": val.get("module_id", "")}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif msg_type == "execute":
        time.sleep(10)
        resp = {"protocol_version": 2, "type": "result", "request_id": req_id, "text": "too late"}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

    #[tokio::test]
    async fn test_execute_timeout() {
        let (desc, dir) = create_fixture_module(TIMEOUT_PY, "timeout");
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        let result = proc.execute("repeat", "test").await;
        assert!(matches!(result, Err(ExternalError::ExecutionTimeout)));
        assert_eq!(proc.status(), ProcessStatus::Crashed);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn test_terminate_process_group() {
        let (desc, dir) = create_child_spawner_module();
        let mut proc = ModuleProcess::start(desc).await.unwrap();
        proc.terminate().await;
        fs::remove_dir_all(&dir).unwrap();
    }
}
