//! A DAP protocol client for connecting to a remote DAP server.
//!
//! The `dap` crate provides a [`Server`] type for reading requests and sending responses,
//! but no client-side equivalent. This module implements a simple DAP client using the
//! same types for requests, responses, and events.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;

use dap::events::Event;
use dap::responses::{Response, ResponseBody};
use dap::types;

/// Variables reference IDs for scopes (must match the DAP server).
pub const SCOPE_STACK: i64 = 1;
pub const SCOPE_MEMORY: i64 = 2;

/// The reason the debuggee stopped after a step/continue command.
#[derive(Debug)]
pub enum DapStopReason {
    /// The debuggee stopped (step, breakpoint, entry, etc.).
    Stopped,
    /// The debuggee terminated.
    Terminated,
}

/// A message received from the DAP server.
#[derive(Debug)]
enum DapMessage {
    Response(Response),
    Event(Event),
}

/// A simple DAP protocol client that communicates over TCP.
pub struct DapClient {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    seq: i64,
}

impl DapClient {
    /// Connect to a DAP server at the given address (e.g. "127.0.0.1:4711").
    pub fn connect(addr: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(addr)
            .map_err(|e| format!("failed to connect to DAP server at {addr}: {e}"))?;
        let reader = BufReader::new(
            stream.try_clone().map_err(|e| format!("failed to clone TCP stream: {e}"))?,
        );
        let writer = BufWriter::new(stream);
        Ok(Self {
            reader,
            writer,
            seq: 0,
        })
    }

    /// Perform the DAP handshake: Initialize + Launch + ConfigurationDone.
    /// Waits for the initial Stopped(entry) event.
    pub fn handshake(&mut self) -> Result<(), String> {
        // Initialize
        self.send_request(
            "initialize",
            serde_json::json!({
                "adapterID": "miden-debug-tui",
                "clientName": "miden-debug TUI",
                "linesStartAt1": true,
                "columnsStartAt1": true,
            }),
        )?;
        self.wait_for_response("initialize")?;

        // Launch
        self.send_request("launch", serde_json::json!({}))?;
        self.wait_for_response("launch")?;

        // ConfigurationDone
        self.send_request("configurationDone", serde_json::json!({}))?;
        self.wait_for_response("configurationDone")?;

        // Wait for Stopped(entry) event
        self.wait_for_stopped()?;
        Ok(())
    }

    /// Send a StepIn command and wait for a Stopped/Terminated event.
    pub fn step_in(&mut self) -> Result<DapStopReason, String> {
        self.send_request("stepIn", serde_json::json!({"threadId": 1}))?;
        self.wait_for_response("stepIn")?;
        self.wait_for_stopped()
    }

    /// Send a Next (step over) command and wait for a Stopped/Terminated event.
    pub fn step_over(&mut self) -> Result<DapStopReason, String> {
        self.send_request("next", serde_json::json!({"threadId": 1}))?;
        self.wait_for_response("next")?;
        self.wait_for_stopped()
    }

    /// Send a StepOut command and wait for a Stopped/Terminated event.
    pub fn step_out(&mut self) -> Result<DapStopReason, String> {
        self.send_request("stepOut", serde_json::json!({"threadId": 1}))?;
        self.wait_for_response("stepOut")?;
        self.wait_for_stopped()
    }

    /// Send a Continue command and wait for a Stopped/Terminated event.
    pub fn continue_(&mut self) -> Result<DapStopReason, String> {
        self.send_request("continue", serde_json::json!({"threadId": 1}))?;
        self.wait_for_response("continue")?;
        self.wait_for_stopped()
    }

    /// Query the current stack trace.
    pub fn stack_trace(&mut self) -> Result<Vec<types::StackFrame>, String> {
        self.send_request("stackTrace", serde_json::json!({"threadId": 1}))?;
        let resp = self.wait_for_response("stackTrace")?;
        match resp.body {
            Some(ResponseBody::StackTrace(st)) => Ok(st.stack_frames),
            _ => Err("unexpected response to stackTrace".into()),
        }
    }

    /// Query variables for a given scope reference.
    pub fn variables(&mut self, variables_reference: i64) -> Result<Vec<types::Variable>, String> {
        self.send_request(
            "variables",
            serde_json::json!({
                "variablesReference": variables_reference
            }),
        )?;
        let resp = self.wait_for_response("variables")?;
        match resp.body {
            Some(ResponseBody::Variables(v)) => Ok(v.variables),
            _ => Err("unexpected response to variables".into()),
        }
    }

    /// Evaluate a custom expression (e.g. "__miden_state").
    pub fn evaluate(&mut self, expression: &str) -> Result<String, String> {
        self.send_request(
            "evaluate",
            serde_json::json!({
                "expression": expression
            }),
        )?;
        let resp = self.wait_for_response("evaluate")?;
        match resp.body {
            Some(ResponseBody::Evaluate(e)) => Ok(e.result),
            _ => Err("unexpected response to evaluate".into()),
        }
    }

    /// Set breakpoints for a source file.
    pub fn set_breakpoints(&mut self, path: &str, lines: &[i64]) -> Result<(), String> {
        let breakpoints: Vec<serde_json::Value> =
            lines.iter().map(|&line| serde_json::json!({"line": line})).collect();
        self.send_request(
            "setBreakpoints",
            serde_json::json!({
                "source": {"path": path},
                "breakpoints": breakpoints,
            }),
        )?;
        self.wait_for_response("setBreakpoints")?;
        Ok(())
    }

    /// Disconnect from the DAP server.
    pub fn disconnect(&mut self) -> Result<(), String> {
        self.send_request("disconnect", serde_json::json!({}))?;
        // Best-effort: try to read response but don't fail if connection closes
        let _ = self.wait_for_response("disconnect");
        Ok(())
    }

    // --- Internal helpers ---

    /// Send a DAP request with Content-Length framing.
    fn send_request(&mut self, command: &str, arguments: serde_json::Value) -> Result<(), String> {
        self.seq += 1;
        let msg = serde_json::json!({
            "seq": self.seq,
            "command": command,
            "arguments": arguments,
        });
        let body = serde_json::to_string(&msg).map_err(|e| format!("serialize error: {e}"))?;
        write!(self.writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)
            .map_err(|e| format!("write error: {e}"))?;
        self.writer.flush().map_err(|e| format!("flush error: {e}"))?;
        Ok(())
    }

    /// Read a single DAP message from the stream (Content-Length framing).
    fn read_message(&mut self) -> Result<DapMessage, String> {
        // Read headers. Skip any blank lines before the header (the server
        // appends `\r\n` after each JSON body, which may appear before the
        // next message's Content-Length header).
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).map_err(|e| format!("read error: {e}"))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if content_length > 0 {
                    // Empty line after a header — end of headers
                    break;
                }
                // Empty line before any header — skip (trailing \r\n from previous message)
                continue;
            }
            if let Some(val) = trimmed.strip_prefix("Content-Length:") {
                content_length = val
                    .trim()
                    .parse::<usize>()
                    .map_err(|e| format!("invalid Content-Length: {e}"))?;
            }
        }

        if content_length == 0 {
            return Err("missing or zero Content-Length header".into());
        }

        // Read content body
        let mut buf = vec![0u8; content_length];
        self.reader.read_exact(&mut buf).map_err(|e| format!("read body error: {e}"))?;
        let content = std::str::from_utf8(&buf).map_err(|e| format!("invalid utf-8: {e}"))?;

        // The server wraps everything in a BaseMessage: { seq, type, ... }
        // We parse the "type" field to determine if it's a response or event.
        let raw: serde_json::Value =
            serde_json::from_str(content).map_err(|e| format!("JSON parse error: {e}"))?;

        match raw.get("type").and_then(|t| t.as_str()) {
            Some("response") => {
                let resp: Response = serde_json::from_value(raw)
                    .map_err(|e| format!("response parse error: {e}"))?;
                Ok(DapMessage::Response(resp))
            }
            Some("event") => {
                let event: Event =
                    serde_json::from_value(raw).map_err(|e| format!("event parse error: {e}"))?;
                Ok(DapMessage::Event(event))
            }
            other => Err(format!("unexpected message type: {other:?}")),
        }
    }

    /// Wait for a response to a specific command, discarding events along the way.
    fn wait_for_response(&mut self, _command: &str) -> Result<Response, String> {
        loop {
            match self.read_message()? {
                DapMessage::Response(resp) => {
                    if !resp.success {
                        let msg = resp
                            .message
                            .as_ref()
                            .map(|m| format!("{m:?}"))
                            .unwrap_or_else(|| "unknown error".into());
                        return Err(format!("DAP error: {msg}"));
                    }
                    return Ok(resp);
                }
                DapMessage::Event(_) => {
                    // Skip events while waiting for response
                    continue;
                }
            }
        }
    }

    /// Wait for a Stopped or Terminated event, discarding other messages.
    fn wait_for_stopped(&mut self) -> Result<DapStopReason, String> {
        loop {
            match self.read_message()? {
                DapMessage::Event(Event::Stopped(_)) => {
                    return Ok(DapStopReason::Stopped);
                }
                DapMessage::Event(Event::Terminated(_)) => {
                    return Ok(DapStopReason::Terminated);
                }
                _ => {
                    // Discard other messages
                    continue;
                }
            }
        }
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}
