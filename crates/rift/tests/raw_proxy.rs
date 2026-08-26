//! One `rift mcp` child reached over its raw stdio pipes, with no rmcp
//! transport in the path.
//!
//! `harness.rs`'s [`crate::harness::proxy_client`] is the entry point every
//! ordinary case uses; this module exists only for a case that must send or
//! observe a frame rmcp's own client cannot construct, such as invalid JSON
//! on the wire. It is declared only where such a case lives, so a suite
//! that never needs it never compiles it.

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

use crate::harness::{TestResult, within};

/// One `rift mcp` child, connected over its raw stdio pipes.
pub(crate) struct RawProxySession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
}

impl RawProxySession {
    /// Spawns the real `rift mcp` binary for `root` and completes the
    /// JSON-RPC initialize handshake by hand.
    pub(crate) async fn connect(root: &Path) -> TestResult<Self> {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rift"));
        command
            .arg("mcp")
            .current_dir(root)
            .env("RUST_LOG", "rift=info,rift_mcp=info,rift_server=info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or("raw proxy stdin missing")?;
        let stdout =
            tokio::io::BufReader::new(child.stdout.take().ok_or("raw proxy stdout missing")?);
        let mut session = Self {
            child,
            stdin,
            stdout,
        };
        session
            .send_line(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"end-to-end-raw","version":"0.0.0"}}}"#,
            )
            .await?;
        let response = session.read_response().await?;
        if response.get("error").is_some() {
            return Err(format!("raw session initialize must not fail: {response}").into());
        }
        session
            .send_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await?;
        Ok(session)
    }

    /// Writes one line verbatim, followed by the newline the stdio
    /// transport reads a frame boundary on.
    pub(crate) async fn send_line(&mut self, raw: &str) -> TestResult {
        self.stdin.write_all(raw.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Reads one response line, bounded so a proxy defect that stops
    /// answering fails the test instead of hanging it.
    pub(crate) async fn read_response(&mut self) -> TestResult<serde_json::Value> {
        let mut line = String::new();
        within("a raw response line", self.stdout.read_line(&mut line)).await??;
        if line.is_empty() {
            return Err("raw proxy stdout closed before a response arrived".into());
        }
        Ok(serde_json::from_str(line.trim_end())?)
    }

    /// Ends the session: closes stdin, then kills this session's own
    /// child by pid and reaps it. Never a `pkill` by name.
    pub(crate) async fn end(mut self) -> TestResult {
        drop(self.stdin);
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        Ok(())
    }
}
