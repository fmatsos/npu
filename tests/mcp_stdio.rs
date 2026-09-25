#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::{
    io::{BufRead, Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn fixture() -> std::path::PathBuf {
    let root = std::path::PathBuf::from("target/test-fixtures").join(format!(
        "mcp-stdio-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join("commands/git")).expect("fixture directory");
    std::fs::write(
        root.join("commands/git/review.md"),
        "---\nmodel = \"missing\"\ndescription = \"Review text\"\n---\n{{ input }}\n",
    )
    .expect("command file");
    root
}

#[test]
fn pipeline_error_format_json_preserves_empty_stdout_and_exit_code() {
    let root = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args([
            "--config-dir",
            root.to_str().expect("UTF-8 path"),
            "git",
            "review",
            "--input",
            "value",
            "--error-format",
            "json",
        ])
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .env("HOME", &root)
        .stdin(Stdio::null())
        .output()
        .expect("business invocation");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("JSON error envelope");
    assert_eq!(error["kind"], "config");
    assert_eq!(error["exit_code"], 2);
}

#[test]
#[allow(clippy::too_many_lines)]
fn tool_calls_reuse_the_backend_pipeline_for_text_and_json() {
    let root = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").expect("local backend");
    let address = listener.local_addr().expect("backend address");
    std::fs::create_dir_all(root.join("backends")).expect("backends directory");
    std::fs::create_dir_all(root.join("models")).expect("models directory");
    std::fs::create_dir_all(root.join("schemas")).expect("schemas directory");
    std::fs::write(root.join("backends/stub.toml"), format!(
        "id = \"stub\"\nbase_url = \"http://{address}\"\ntype = \"openai-compatible\"\n[operations.chat]\nmethod = \"POST\"\npath = \"/v1/chat/completions\"\n"
    )).expect("backend file");
    std::fs::write(
        root.join("models/test.toml"),
        "id = \"test\"\nbackend = \"stub\"\noperation = \"chat\"\nmodel = \"test\"\n",
    )
    .expect("model file");
    std::fs::write(
        root.join("commands/git/review.md"),
        "---\nmodel = \"test\"\n[args.input]\nrequired = true\n---\n{{ args.input }} {{ input }}\n",
    )
    .expect("text command");
    std::fs::write(root.join("commands/json.md"), "---\nmodel = \"test\"\n[output]\nformat = \"json\"\nschema = \"schemas/result.json\"\n---\n{{ input }}\n").expect("json command");
    std::fs::write(
        root.join("schemas/result.json"),
        r#"{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"]}"#,
    )
    .expect("schema file");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let backend = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for _ in 0..2 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "backend request not received"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(err) => panic!("backend accept: {err}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("read timeout");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().expect("content length");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            let body = String::from_utf8(body).expect("UTF-8 request");
            let response_text = if body.contains("json input") {
                "{\"ok\":true}"
            } else {
                "reviewed"
            };
            bodies.push(body);
            let payload = serde_json::json!({"choices":[{"message":{"role":"assistant","content":response_text}}]}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).expect("response");
        }
        bodies
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args([
            "--config-dir",
            root.to_str().expect("UTF-8 path"),
            "mcp",
            "serve",
        ])
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .env("HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("server starts");
    let meta = serde_json::json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}})).expect("discover");
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"git_review","arguments":{"input":"arg value","mcp.input":"body value"},"_meta":meta}})).expect("text call");
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"json","arguments":{"mcp.input":"json input"},"_meta":meta}})).expect("json call");
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("server exits");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("protocol UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON response"))
        .collect();
    let by_id = |id| {
        responses
            .iter()
            .find(|response| response["id"] == id)
            .expect("response id")
    };
    assert_eq!(by_id(2)["result"]["isError"], false, "{responses:?}");
    assert_eq!(by_id(2)["result"]["content"][0]["text"], "reviewed");
    assert_eq!(by_id(3)["result"]["structuredContent"]["ok"], true);
    let bodies = backend.join().expect("backend thread");
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("arg value body value"))
    );
}

#[test]
fn discovers_tools_and_rejects_invalid_call_without_stdout_noise() {
    let root = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_npu"))
        .args([
            "--config-dir",
            root.to_str().expect("utf-8 path"),
            "mcp",
            "serve",
        ])
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .env("HOME", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("server starts");
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let messages = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":meta}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"git_review","arguments":{"unknown":"x"},"_meta":meta}}),
    ];
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for message in messages {
            writeln!(stdin, "{message}").expect("request");
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("server exits on EOF");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .expect("utf-8 protocol")
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON response per line"))
        .collect();
    assert_eq!(responses.len(), 3);
    let by_id = |id| {
        responses
            .iter()
            .find(|response| response["id"] == id)
            .expect("response id")
    };
    assert_eq!(
        by_id(1)["result"]["supportedVersions"],
        serde_json::json!(["2026-07-28"])
    );
    assert_eq!(
        by_id(1)["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "npu"
    );
    assert_eq!(by_id(2)["result"]["tools"][0]["name"], "git_review");
    assert!(by_id(3).get("error").is_some());
}
