//! A DAP protocol client for connecting to a remote DAP server.
//!
//! The `dap` crate provides a [`Server`] type for reading requests and sending responses,
//! but no client-side equivalent. This module implements a simple DAP client using the
//! same types for requests, responses, and events.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    net::TcpStream,
};

use dap::{
    events::Event,
    responses::{Response, ResponseBody},
    types,
};

use super::DapUiState;
use crate::debug::ReadMemoryExpr;

/// Variables reference IDs for scopes (must match the DAP server).
pub const SCOPE_STACK: i64 = 1;
pub const SCOPE_MEMORY: i64 = 2;

/// The reason the debuggee stopped after a step/continue command.
#[derive(Debug)]
pub enum DapStopReason {
    /// The debuggee stopped (step, breakpoint, entry, etc.) and the server
    /// pushed a bundled UI state snapshot via the custom `miden/uiState` event.
    Stopped(DapUiState),
    /// The debuggee terminated.
    Terminated,
    /// Phase 2: server signaled terminate-and-reconnect.
    Restarting,
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
    pending_events: VecDeque<Event>,
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
            pending_events: VecDeque::new(),
            seq: 0,
        })
    }

    /// Perform the DAP handshake: Initialize + Launch + ConfigurationDone.
    /// Waits for the initial Stopped(entry) event and returns the server-pushed
    /// UI state snapshot.
    pub fn handshake(&mut self) -> Result<DapUiState, String> {
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

        // Wait for Stopped(entry) event plus the pushed UI state snapshot.
        match self.wait_for_stopped()? {
            DapStopReason::Stopped(snapshot) => Ok(snapshot),
            DapStopReason::Terminated => Err("server terminated before entry stop".into()),
            DapStopReason::Restarting => Err("server requested restart during handshake".into()),
        }
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

    /// Read memory from the remote debuggee via the DAP server.
    pub fn read_memory(&mut self, expr: &ReadMemoryExpr) -> Result<String, String> {
        self.evaluate(&format!("__miden_read_memory {expr}"))
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

    /// Set function breakpoints (matched as glob patterns against context names and file paths).
    pub fn set_function_breakpoints(&mut self, names: &[String]) -> Result<(), String> {
        let breakpoints: Vec<serde_json::Value> =
            names.iter().map(|name| serde_json::json!({"name": name})).collect();
        self.send_request(
            "setFunctionBreakpoints",
            serde_json::json!({
                "breakpoints": breakpoints,
            }),
        )?;
        self.wait_for_response("setFunctionBreakpoints")?;
        Ok(())
    }

    /// Send a Restart command and wait for a Stopped event (program restarted at entry).
    ///
    /// The server resets the processor to the beginning of the program with the same
    /// inputs and re-emits `miden/uiState` + `Stopped(entry)`.
    pub fn restart(&mut self) -> Result<DapStopReason, String> {
        self.send_request("restart", serde_json::json!({}))?;
        self.wait_for_response("restart")?;
        self.wait_for_stopped()
    }

    /// Send a Phase 2 restart command (with arguments) and wait for the server's response.
    ///
    /// The server will respond with `Terminated(restart=true)` and shut down so the caller
    /// can recompile and reconnect.
    pub fn restart_phase2(&mut self) -> Result<DapStopReason, String> {
        self.send_request("restart", serde_json::json!({"arguments": {}}))?;
        self.wait_for_response("restart")?;
        self.wait_for_stopped()
    }

    /// Connect to a DAP server with exponential backoff, for Phase 2 reconnection.
    ///
    /// Polls with delays from 50ms up to 1s, timing out after `timeout`.
    pub fn connect_with_retry(addr: &str, timeout: std::time::Duration) -> Result<Self, String> {
        let start = std::time::Instant::now();
        let mut delay = std::time::Duration::from_millis(50);
        loop {
            match TcpStream::connect(addr) {
                Ok(stream) => {
                    let reader = BufReader::new(
                        stream
                            .try_clone()
                            .map_err(|e| format!("failed to clone TCP stream: {e}"))?,
                    );
                    let writer = BufWriter::new(stream);
                    return Ok(Self {
                        reader,
                        writer,
                        pending_events: VecDeque::new(),
                        seq: 0,
                    });
                }
                Err(_) if start.elapsed() < timeout => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_secs(1));
                }
                Err(e) => {
                    return Err(format!(
                        "failed to reconnect to {addr} after {:.1}s: {e}",
                        timeout.as_secs_f64()
                    ));
                }
            }
        }
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
        // The length comes from whatever this client is connected to, and feeds the allocation
        // below directly, so an unbounded value would end the process rather than the read.
        if content_length > dap::server::MAX_CONTENT_LENGTH {
            return Err(format!(
                "Content-Length {content_length} exceeds the maximum of {}",
                dap::server::MAX_CONTENT_LENGTH
            ));
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

    /// Read the next message, replaying events buffered while waiting for responses first.
    fn next_message(&mut self) -> Result<DapMessage, String> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(DapMessage::Event(event));
        }

        self.read_message()
    }

    /// Wait for a response to a specific command, preserving events seen along the way.
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
                DapMessage::Event(event) => {
                    // The adapter may emit meaningful state events before the response
                    // we are waiting for. Preserve them so the following wait_for_stopped
                    // call can consume the stop/UI-state pair instead of hanging.
                    self.pending_events.push_back(event);
                    continue;
                }
            }
        }
    }

    /// Wait for a Stopped or Terminated event, capturing the server-pushed
    /// `miden/uiState` snapshot that arrives before the stop signal.
    ///
    /// The server emits the custom `MidenUiState` event immediately before
    /// the standard `Stopped` event, so this loop naturally captures it.
    fn wait_for_stopped(&mut self) -> Result<DapStopReason, String> {
        let mut snapshot: Option<DapUiState> = None;
        loop {
            match self.next_message()? {
                DapMessage::Event(Event::Stopped(_)) => {
                    let snapshot = snapshot.ok_or_else(|| {
                        "DAP server sent 'stopped' without a preceding miden/uiState event; the \
                         remote debugger does not appear to be a miden-debug DAP server"
                            .to_string()
                    })?;
                    return Ok(DapStopReason::Stopped(snapshot));
                }
                DapMessage::Event(Event::Terminated(body)) => {
                    let is_restart = body
                        .as_ref()
                        .and_then(|b| b.restart.as_ref())
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if is_restart {
                        return Ok(DapStopReason::Restarting);
                    }
                    return Ok(DapStopReason::Terminated);
                }
                DapMessage::Event(Event::MidenUiState(value)) => {
                    snapshot = Some(
                        serde_json::from_value::<DapUiState>(value)
                            .map_err(|e| format!("invalid miden/uiState payload: {e}"))?,
                    );
                }
                _ => continue,
            }
        }
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Shutdown, TcpListener},
        time::Duration,
    };

    use serde_json::{Value, json};

    use super::*;

    struct Connection {
        client: DapClient,
        peer: TcpStream,
        peer_reader: BufReader<TcpStream>,
    }

    impl Connection {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = DapClient::connect(&listener.local_addr().unwrap().to_string()).unwrap();
            let (peer, _) = listener.accept().unwrap();
            client.reader.get_ref().set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let peer_reader = BufReader::new(peer.try_clone().unwrap());
            Self {
                client,
                peer,
                peer_reader,
            }
        }

        fn send(&mut self, messages: &[Value]) {
            for message in messages {
                let payload = message.to_string();
                write!(self.peer, "Content-Length: {}\r\n\r\n{payload}\r\n", payload.len())
                    .unwrap();
            }
        }

        fn request(&mut self) -> Value {
            let reader = &mut self.peer_reader;
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let length: usize =
                header.trim().strip_prefix("Content-Length:").unwrap().trim().parse().unwrap();
            header.clear();
            reader.read_line(&mut header).unwrap();
            assert_eq!(header, "\r\n");
            let mut payload = vec![0; length];
            reader.read_exact(&mut payload).unwrap();
            serde_json::from_slice(&payload).unwrap()
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            let _ = self.client.writer.get_ref().shutdown(Shutdown::Both);
        }
    }

    fn response(command: &str, body: Value) -> Value {
        json!({"type": "response", "seq": 1, "request_seq": 1, "success": true, "command": command, "body": body, "error": null})
    }

    fn stopped(cycle: usize) -> [Value; 2] {
        [
            json!({"type": "event", "event": "miden/uiState", "body": {"cycle": cycle, "current_stack": [7], "callstack": []}}),
            json!({"type": "event", "event": "stopped", "body": {"reason": "step", "threadId": 1}}),
        ]
    }

    #[test]
    fn handshake_preserves_state_events_received_before_responses() {
        let mut connection = Connection::new();
        let [state, event] = stopped(3);
        connection.send(&[
            response("initialize", json!({})),
            response("launch", json!(null)),
            state,
            response("configurationDone", json!(null)),
            event,
        ]);
        let snapshot = connection.client.handshake().unwrap();
        assert_eq!(snapshot.cycle, 3);
        assert_eq!(snapshot.current_stack, [7]);
        assert!(connection.client.pending_events.is_empty());
        assert_eq!(connection.client.seq, 3);
        assert_eq!(connection.request()["command"], "initialize");
    }

    #[test]
    fn stepping_and_restarting_send_expected_commands_and_decode_stop_reasons() {
        for (command, operation) in [
            (
                "stepIn",
                DapClient::step_in as fn(&mut DapClient) -> Result<DapStopReason, String>,
            ),
            ("next", DapClient::step_over),
            ("stepOut", DapClient::step_out),
            ("continue", DapClient::continue_),
            ("restart", DapClient::restart),
        ] {
            let mut connection = Connection::new();
            let [state, event] = stopped(7);
            connection.send(&[response(command, json!(null)), state, event]);
            assert!(
                matches!(operation(&mut connection.client).unwrap(), DapStopReason::Stopped(snapshot) if snapshot.cycle == 7)
            );
            let request = connection.request();
            assert_eq!(request["command"], command);
            assert_eq!(request["seq"], 1);
            if command != "restart" {
                assert_eq!(request["arguments"]["threadId"], 1);
            }
        }
        let mut connection = Connection::new();
        connection.send(&[
            response("restart", json!(null)),
            json!({"type": "event", "event": "terminated", "body": {"restart": true}}),
        ]);
        assert!(matches!(connection.client.restart_phase2().unwrap(), DapStopReason::Restarting));
        assert!(connection.request()["arguments"]["arguments"].is_object());
        connection.send(&[
            response("continue", json!({})),
            json!({"type": "event", "event": "terminated"}),
        ]);
        assert!(matches!(connection.client.continue_().unwrap(), DapStopReason::Terminated));
        assert_eq!(connection.request()["seq"], 2);
    }

    #[test]
    fn queries_and_breakpoints_round_trip_remote_payloads() {
        let mut connection = Connection::new();
        connection.send(&[response("stackTrace", json!({"stackFrames": [{"id": 0, "name": "main", "line": 4, "column": 1}], "totalFrames": 1}))]);
        assert_eq!(connection.client.stack_trace().unwrap()[0].name.as_ref(), "main");
        assert_eq!(connection.request()["command"], "stackTrace");
        connection.send(&[response(
            "variables",
            json!({"variables": [{"name": "input", "value": "5", "variablesReference": 0}]}),
        )]);
        assert_eq!(connection.client.variables(SCOPE_STACK).unwrap()[0].value, "5");
        assert_eq!(connection.request()["arguments"]["variablesReference"], SCOPE_STACK);
        connection.send(&[response("evaluate", json!({"result": "42", "variablesReference": 0}))]);
        assert_eq!(connection.client.read_memory(&"0 -t u32".parse().unwrap()).unwrap(), "42");
        assert!(
            connection.request()["arguments"]["expression"]
                .as_str()
                .unwrap()
                .starts_with("__miden_read_memory ")
        );
        connection.send(&[response("setBreakpoints", json!({"breakpoints": []}))]);
        connection.client.set_breakpoints("main.rs", &[3, 7]).unwrap();
        let request = connection.request();
        assert_eq!(request["arguments"]["source"]["path"], "main.rs");
        assert_eq!(request["arguments"]["breakpoints"], json!([{"line": 3}, {"line": 7}]));
        connection.send(&[response("setFunctionBreakpoints", json!({"breakpoints": []}))]);
        connection.client.set_function_breakpoints(&["main".into()]).unwrap();
        assert_eq!(connection.request()["arguments"]["breakpoints"][0]["name"], "main");
        connection.send(&[response("disconnect", json!(null))]);
        connection.client.disconnect().unwrap();
        assert_eq!(connection.request()["command"], "disconnect");
    }

    #[test]
    fn invalid_responses_and_stop_events_report_actionable_errors() {
        for (message, expected) in [
            (
                json!({"type": "response", "request_seq": 1, "success": false, "message": "bad request", "error": null}),
                "DAP error",
            ),
            (
                json!({"type": "response", "request_seq": 1, "success": false, "error": null}),
                "unknown error",
            ),
            (json!({"type": "request"}), "unexpected message type"),
        ] {
            let mut connection = Connection::new();
            connection.send(&[message]);
            assert!(
                connection.client.wait_for_response("evaluate").unwrap_err().contains(expected)
            );
        }
        for (message, expected) in [
            (
                json!({"type": "event", "event": "stopped", "body": {"reason": "step"}}),
                "without a preceding",
            ),
            (
                json!({"type": "event", "event": "miden/uiState", "body": {}}),
                "invalid miden/uiState",
            ),
        ] {
            let mut connection = Connection::new();
            connection.send(&[message]);
            assert!(connection.client.wait_for_stopped().unwrap_err().contains(expected));
        }
        let mut connection = Connection::new();
        connection.send(&[response("continue", json!({}))]);
        assert!(connection.client.stack_trace().unwrap_err().contains("unexpected response"));
        connection.send(&[response("continue", json!({}))]);
        assert!(connection.client.variables(1).unwrap_err().contains("unexpected response"));
        connection.send(&[response("continue", json!({}))]);
        assert!(connection.client.evaluate("input").unwrap_err().contains("unexpected response"));
    }

    #[test]
    fn framing_rejects_invalid_lengths_utf8_and_json() {
        for (wire, expected) in [
            (b"Content-Length: nope\r\n\r\n".to_vec(), "invalid Content-Length"),
            (
                format!("Content-Length: {}\r\n\r\n", dap::server::MAX_CONTENT_LENGTH + 1)
                    .into_bytes(),
                "exceeds the maximum",
            ),
            (b"Content-Length: 1\r\n\r\n\xff".to_vec(), "invalid utf-8"),
            (b"Content-Length: 1\r\n\r\nx".to_vec(), "JSON parse error"),
        ] {
            let mut connection = Connection::new();
            connection.peer.write_all(&wire).unwrap();
            assert!(connection.client.read_message().unwrap_err().contains(expected));
        }
    }

    #[test]
    fn retry_connects_to_available_listener_and_reports_invalid_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = DapClient::connect_with_retry(
            &listener.local_addr().unwrap().to_string(),
            Duration::ZERO,
        )
        .unwrap();
        let (_peer, _) = listener.accept().unwrap();
        client.writer.get_ref().shutdown(Shutdown::Both).unwrap();
        assert!(DapClient::connect("invalid address").is_err());
        assert!(
            DapClient::connect_with_retry("invalid address", Duration::ZERO)
                .err()
                .unwrap()
                .contains("failed to reconnect")
        );
    }
}
