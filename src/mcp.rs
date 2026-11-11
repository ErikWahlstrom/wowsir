//! `wowsir mcp`: a Model Context Protocol server over stdio.
//!
//! Deliberately tiny surface (two tools) so it costs little context. Each
//! call runs the very same `wowsir` command line the CLI would, so the CLI and
//! MCP can never drift apart: every CLI command is available over MCP.

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

const BROWSER_DESC: &str = r#"Headless browser for testing web pages (wowsir). Pass one wowsir command; the browser and session persist between calls.
After each action wowsir waits for the page to settle and returns: `ok <action> · <ms>`, `→ <new url>`, `! <console/network errors, dialogs>`, and `~` a diff of the page (+ added, - removed). Act on refs from the output; you rarely need a fresh snapshot.
Commands:
  open <url> | back | reload | tabs | tab N | tab close | snap [-i] [--grep TEXT] [--in TARGET] [--viewport]
  click T [--mod|--shift] | fill T TEXT | type [T] TEXT | press KEY (Enter, Control+a) | select T OPTION | check T | uncheck T | hover T | scroll [down|up|T] | scroll T down|PX (inside it) | upload T FILE
  wait text TEXT | wait gone T | wait url PART
  expect text|no-text TEXT | expect url PART | expect visible|hidden T | expect value T V | expect count T N | expect no-errors
  text [T] | console [--errors] | net [--failed] | net --body PATTERN | errors | eval JS | screenshot [PATH] | viewport W H | viewport mobile
  watch T... (then actions report "! moved:/jumped:") | expect still | screenshot --mark
  mock PATTERN 500|abort [--json X] [--delay MS] | mock off
  run FILE... [--parallel N] | record save FILE | logout SITE | login URL (opens a window for the human to sign in; only when wowsir says sign-in is needed)
Targets: @e12 (ref) · button:Sign in (role:name) · text=Forgot password · CSS selector · A >> B (B near A, e.g. "h3:Billing >> button:Edit").
Flags: -q (no diff) · --no-auth (logged-out session) · --timeout MS."#;

const RUN_DESC: &str = "Run several wowsir commands in one call, one per line (# comments allowed). Stops at the first failure and reports the failing line. Use for multi-step flows or to replay a recorded test.";

fn tools() -> Value {
    json!([
        {
            "name": "browser",
            "description": BROWSER_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "A wowsir command, e.g. `open example.com`, `click @e5`, `fill textbox:Email a@b.c`, `expect text \"Saved\"`." },
                    "session": { "type": "string", "description": "Optional session name: isolated cookies/storage, for parallel or clean tests. Default: shared with the wowsir CLI." }
                },
                "required": ["command"]
            }
        },
        {
            "name": "browser_run",
            "description": RUN_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "steps": { "type": "string", "description": "wowsir commands, one per line." },
                    "session": { "type": "string" }
                },
                "required": ["steps"]
            }
        }
    ])
}

/// Run `wowsir [-s session] <args>` (optionally with stdin) and return (output, ok).
fn run_wowsir(args: Vec<String>, session: Option<&str>, stdin: Option<&str>) -> (String, bool) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "wowsir".into());
    let mut cmd = Command::new(exe);
    if let Some(s) = session.filter(|s| !s.is_empty()) {
        cmd.args(["-s", s]);
    }
    cmd.args(&args).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (format!("FAIL could not run wowsir: {e}"), false),
    };
    if let (Some(input), Some(mut si)) = (stdin, child.stdin.take()) {
        let _ = si.write_all(input.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(err.trim_end());
            }
            (text, o.status.success())
        }
        Err(e) => (format!("FAIL {e}"), false),
    }
}

fn call_tool(name: &str, args: &Value) -> (String, bool) {
    let session = args["session"].as_str();
    match name {
        "browser" => {
            let command = args["command"].as_str().unwrap_or("").trim();
            let command = command.strip_prefix("wowsir ").unwrap_or(command);
            if command.is_empty() {
                return ("FAIL empty command. Example: open example.com".into(), false);
            }
            if let Some(w) = crate::commands::eval_line(command) {
                return run_wowsir(vec![w[0].clone(), "-".into()], session, Some(&w[1]));
            }
            match crate::commands::split_line(command) {
                Ok(argv) if argv.first().map(|s| s.as_str()) == Some("mcp") => ("FAIL not available over MCP".into(), false),
                Ok(argv) => run_wowsir(argv, session, None),
                Err(e) => (format!("FAIL could not parse command ({e}). Quote text with spaces: fill @e3 \"hello world\""), false),
            }
        }
        "browser_run" => run_wowsir(vec!["run".into(), "-".into()], session, Some(args["steps"].as_str().unwrap_or(""))),
        _ => (format!("unknown tool {name}"), false),
    }
}

pub fn serve() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let method = msg["method"].as_str().unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "wowsir", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "wowsir is a fast headless browser for testing web pages. Use the `browser` tool with wowsir commands; every action reports what changed plus any console/network errors."
            }),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let (text, ok) = call_tool(msg["params"]["name"].as_str().unwrap_or(""), &msg["params"]["arguments"]);
                json!({ "content": [{ "type": "text", "text": text }], "isError": !ok })
            }
            _ => {
                let err = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {method}") } });
                let _ = writeln!(out, "{err}");
                let _ = out.flush();
                continue;
            }
        };
        let _ = writeln!(out, "{}", json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        let _ = out.flush();
    }
}
