//! stdio proxy between an MCP client and `mnemosyne mcp`.
//!
//! Mnemosyne only uses the bank passed on the command line as a *default*:
//! every tool call may override it with its own `bank` argument, and a few
//! tools (`mnemosyne_shared_*`, `mnemosyne_sync_*`) reach outside the project
//! bank altogether. This proxy removes both escape hatches:
//!
//! * every `tools/call` gets `arguments.bank` forced to the configured bank;
//! * the shared/sync tools are hidden from `tools/list` and refused on call.
//!
//! Usage: `mnemosyne-guard --bank <name>`. The Mnemosyne binary is taken from
//! `MNEMOSYNE_BIN`, falling back to `~/.local/bin/mnemosyne`.

use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

const DENIED_PREFIXES: [&str; 2] = ["mnemosyne_shared_", "mnemosyne_sync_"];

type Out = Arc<Mutex<io::Stdout>>;

enum Verdict {
    Forward(Value),
    Reply(Value),
}

fn is_denied(tool: &str) -> bool {
    DENIED_PREFIXES
        .iter()
        .any(|prefix| tool.starts_with(prefix))
}

fn error_reply(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Client -> server.
fn guard_request(mut msg: Value, bank: &str) -> Verdict {
    if msg.get("method").and_then(Value::as_str) != Some("tools/call") {
        return Verdict::Forward(msg);
    }

    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let Some(params) = msg.get("params").and_then(Value::as_object) else {
        return Verdict::Reply(error_reply(
            id,
            -32600,
            "tools/call params must be an object".into(),
        ));
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Verdict::Reply(error_reply(
            id,
            -32600,
            "tools/call params.name must be a string".into(),
        ));
    };

    if is_denied(name) {
        let message = format!("tool {name} is disabled by mnemosyne-guard");
        return Verdict::Reply(error_reply(id, -32601, message));
    }

    let params = msg["params"]
        .as_object_mut()
        .expect("params was checked as an object");
    let args = params.entry("arguments").or_insert_with(|| json!({}));
    if !args.is_object() {
        *args = json!({});
    }
    args["bank"] = json!(bank);

    Verdict::Forward(msg)
}

/// Разобрать одну строку от клиента и решить, что с ней делать.
fn guard_line(line: &str, bank: &str) -> Verdict {
    match serde_json::from_str::<Value>(line) {
        // A batch could smuggle calls past the per-message checks.
        Ok(Value::Array(_)) => Verdict::Reply(error_reply(
            Value::Null,
            -32600,
            "batch requests are not supported by mnemosyne-guard".into(),
        )),
        Ok(msg) => guard_request(msg, bank),
        // Do not forward unparseable data: a softer server parser could
        // otherwise accept a message the guard never inspected.
        Err(_) => Verdict::Reply(error_reply(Value::Null, -32700, "parse error".into())),
    }
}

/// Server -> client.
fn guard_response(mut msg: Value) -> Value {
    if let Some(tools) = msg
        .pointer_mut("/result/tools")
        .and_then(Value::as_array_mut)
    {
        tools.retain(|tool| {
            !tool
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(is_denied)
        });
    }
    msg
}

fn send(out: &Out, line: &str) {
    let mut out = out.lock().unwrap();
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

fn bank_from_args() -> Option<String> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next(), args.next()) {
        (Some("--bank"), Some(bank), None) if !bank.is_empty() => Some(bank),
        _ => None,
    }
}

fn mnemosyne_bin() -> String {
    std::env::var("MNEMOSYNE_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.local/bin/mnemosyne")
    })
}

fn main() -> ExitCode {
    let Some(bank) = bank_from_args() else {
        eprintln!("usage: mnemosyne-guard --bank <name>");
        return ExitCode::from(2);
    };

    let mut child = match Command::new(mnemosyne_bin())
        .args(["mcp", "--bank", &bank])
        .env("MNEMOSYNE_BANK", &bank)
        .env("MNEMOSYNE_MCP_BANK", &bank)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("mnemosyne-guard: cannot start {}: {error}", mnemosyne_bin());
            return ExitCode::from(1);
        }
    };

    let mut child_stdin = child.stdin.take().expect("piped stdin");
    let child_stdout = child.stdout.take().expect("piped stdout");
    let out: Out = Arc::new(Mutex::new(io::stdout()));

    // Client -> server. Dropping `child_stdin` on EOF lets the server exit.
    let client_out = Arc::clone(&out);
    thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let forward = match guard_line(&line, &bank) {
                Verdict::Forward(msg) => msg.to_string(),
                Verdict::Reply(reply) => {
                    send(&client_out, &reply.to_string());
                    continue;
                }
            };
            if writeln!(child_stdin, "{forward}")
                .and_then(|()| child_stdin.flush())
                .is_err()
            {
                break;
            }
        }
    });

    // Server -> client, until the server closes its stdout.
    for line in BufReader::new(child_stdout).lines().map_while(Result::ok) {
        match serde_json::from_str::<Value>(&line) {
            Ok(msg) => send(&out, &guard_response(msg).to_string()),
            Err(_) => send(&out, &line),
        }
    }

    match child.wait() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": { "name": name, "arguments": args } })
    }

    fn forwarded(verdict: Verdict) -> Value {
        match verdict {
            Verdict::Forward(msg) => msg,
            Verdict::Reply(reply) => panic!("expected forward, got reply {reply}"),
        }
    }

    #[test]
    fn overrides_a_foreign_bank() {
        let msg = forwarded(guard_request(
            call("mnemosyne_recall", json!({ "bank": "ds", "query": "x" })),
            "tasklerq",
        ));
        assert_eq!(msg["params"]["arguments"]["bank"], "tasklerq");
        assert_eq!(msg["params"]["arguments"]["query"], "x");
    }

    #[test]
    fn adds_bank_when_arguments_missing_or_not_an_object() {
        let missing = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                              "params": { "name": "mnemosyne_stats" } });
        let msg = forwarded(guard_request(missing, "tasklerq"));
        assert_eq!(msg["params"]["arguments"]["bank"], "tasklerq");

        let msg = forwarded(guard_request(
            call("mnemosyne_stats", json!("oops")),
            "tasklerq",
        ));
        assert_eq!(msg["params"]["arguments"]["bank"], "tasklerq");
    }

    #[test]
    fn refuses_shared_and_sync_tools() {
        for name in ["mnemosyne_shared_remember", "mnemosyne_sync_push"] {
            match guard_request(call(name, json!({})), "tasklerq") {
                Verdict::Reply(reply) => {
                    assert_eq!(reply["id"], 7);
                    assert_eq!(reply["error"]["code"], -32601);
                }
                Verdict::Forward(_) => panic!("{name} must be refused"),
            }
        }
    }

    #[test]
    fn leaves_other_methods_untouched() {
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        assert_eq!(forwarded(guard_request(init.clone(), "tasklerq")), init);
    }

    #[test]
    fn rejects_batch_requests() {
        let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call"}]"#;
        match guard_line(batch, "tasklerq") {
            Verdict::Reply(reply) => {
                assert_eq!(reply["id"], Value::Null);
                assert_eq!(reply["error"]["code"], -32600);
            }
            Verdict::Forward(_) => panic!("batch must not be forwarded"),
        }
    }

    #[test]
    fn rejects_non_json_without_forwarding() {
        match guard_line("this is not json", "tasklerq") {
            Verdict::Reply(reply) => {
                assert_eq!(reply["id"], Value::Null);
                assert_eq!(reply["error"]["code"], -32700);
            }
            Verdict::Forward(_) => panic!("non-JSON must not be forwarded"),
        }
    }

    #[test]
    fn invalid_tools_call_preserves_id() {
        let cases = [
            (
                json!({ "jsonrpc": "2.0", "id": "array-params", "method": "tools/call",
                        "params": [] }),
                "array-params",
            ),
            (
                json!({ "jsonrpc": "2.0", "id": "numeric-name", "method": "tools/call",
                        "params": { "name": 42 } }),
                "numeric-name",
            ),
        ];

        for (msg, expected_id) in cases {
            match guard_request(msg, "tasklerq") {
                Verdict::Reply(reply) => {
                    assert_eq!(reply["id"], expected_id);
                    assert_eq!(reply["error"]["code"], -32600);
                }
                Verdict::Forward(_) => panic!("invalid tools/call must be refused"),
            }
        }
    }

    #[test]
    fn hides_denied_tools_from_the_listing() {
        let listing = json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": [
            { "name": "mnemosyne_recall" },
            { "name": "mnemosyne_shared_recall" },
            { "name": "mnemosyne_sync_status" },
            { "name": "mnemosyne_stats" },
        ] } });
        let names: Vec<_> = guard_response(listing)["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["mnemosyne_recall", "mnemosyne_stats"]);
    }
}
