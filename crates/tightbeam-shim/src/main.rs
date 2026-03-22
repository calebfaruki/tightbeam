use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process;

const SOCKET_PATH: &str = "/run/docker-tightbeam.sock";

#[derive(Deserialize)]
struct Response {
    id: Option<u64>,
    method: Option<String>,
    params: Option<OutputParams>,
    #[serde(rename = "result")]
    _result: Option<serde_json::Value>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct OutputParams {
    stream: String,
    data: StreamData,
}

#[derive(Deserialize)]
struct StreamData {
    #[serde(rename = "type")]
    _data_type: String,
    text: Option<String>,
    #[serde(rename = "id")]
    _id: Option<String>,
    #[serde(rename = "name")]
    _name: Option<String>,
    #[serde(rename = "input")]
    _input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i32,
    message: String,
}

fn read_response_loop(
    socket_reader: &mut impl BufRead,
    stdout: &std::io::Stdout,
) -> Result<(), i32> {
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        match socket_reader.read_line(&mut line_buf) {
            Ok(0) => {
                eprintln!("tightbeam: daemon disconnected mid-response");
                return Err(1);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("tightbeam: daemon connection lost: {e}");
                return Err(1);
            }
        }

        let line = line_buf.trim_end();
        if line.is_empty() {
            continue;
        }

        let resp: Response = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Streaming notification (no id) — forward to stdout
        if resp.method.as_deref() == Some("output") {
            if let Some(params) = &resp.params {
                if params.stream == "content" {
                    if let Some(ref text) = params.data.text {
                        let mut out = stdout.lock();
                        let _ = out.write_all(text.as_bytes());
                        let _ = out.flush();
                    }
                }
            }
            let mut out = stdout.lock();
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
            continue;
        }

        // Final response (has id) — forward and return
        if resp.id.is_some() {
            let mut out = stdout.lock();
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();

            if let Some(error) = &resp.error {
                eprintln!("tightbeam: error {} — {}", error.code, error.message);
                return Err(1);
            }

            return Ok(());
        }
    }
}

fn main() {
    let stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tightbeam: cannot connect to daemon at {SOCKET_PATH}: {e}");
            process::exit(1);
        }
    };

    let stdin = std::io::stdin();
    let stdin_reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();

    let mut socket_writer = stream.try_clone().unwrap_or_else(|e| {
        eprintln!("tightbeam: failed to clone socket: {e}");
        process::exit(1);
    });
    let mut socket_reader = BufReader::new(stream);

    for line in stdin_reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("tightbeam: failed to read stdin: {e}");
                process::exit(1);
            }
        };

        if line.is_empty() {
            continue;
        }

        // Send request to daemon
        let mut payload = line;
        payload.push('\n');
        if let Err(e) = socket_writer.write_all(payload.as_bytes()) {
            eprintln!("tightbeam: daemon connection lost (broken pipe): {e}");
            process::exit(1);
        }
        if let Err(e) = socket_writer.flush() {
            eprintln!("tightbeam: daemon connection lost (broken pipe): {e}");
            process::exit(1);
        }

        // Read streaming notifications + final response
        if let Err(code) = read_response_loop(&mut socket_reader, &stdout) {
            process::exit(code);
        }
    }

    // stdin exhausted — clean exit
}

#[cfg(test)]
mod multi_turn {
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn shim_handles_multi_turn_conversation() {
        let sock_dir = std::env::temp_dir().join(format!("tb-mt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        std::fs::create_dir_all(&sock_dir).unwrap();
        let sock_path = sock_dir.join("t.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn a mock daemon that handles two requests
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;

            // Request 1: read request, send notification + final response
            let mut req1 = String::new();
            std::io::BufRead::read_line(&mut reader, &mut req1).unwrap();
            assert!(req1.contains("llm_call"));

            let notif = r#"{"jsonrpc":"2.0","method":"output","params":{"stream":"content","data":{"type":"text","text":"Hello"}}}"#;
            writer.write_all(notif.as_bytes()).unwrap();
            writer.write_all(b"\n").unwrap();

            let final_resp = r#"{"jsonrpc":"2.0","id":1,"result":{"stop_reason":"tool_use","tool_calls":[{"id":"tc-1","name":"bash","input":{"command":"ls"}}]}}"#;
            writer.write_all(final_resp.as_bytes()).unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();

            // Request 2: read tool_result, send final response
            let mut req2 = String::new();
            std::io::BufRead::read_line(&mut reader, &mut req2).unwrap();
            assert!(req2.contains("tool_result"));

            let final_resp2 =
                r#"{"jsonrpc":"2.0","id":2,"result":{"stop_reason":"end_turn","text":"Done."}}"#;
            writer.write_all(final_resp2.as_bytes()).unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();
        });

        // Client side: connect and simulate two requests
        let stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let mut client_writer = stream.try_clone().unwrap();
        let mut client_reader = BufReader::new(stream);

        // Send request 1
        let req1 = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"Hello"}],"tools":[]}}"#;
        client_writer
            .write_all(format!("{req1}\n").as_bytes())
            .unwrap();
        client_writer.flush().unwrap();

        // Read responses for request 1
        let stdout = std::io::stdout();
        let result = super::read_response_loop(&mut client_reader, &stdout);
        assert!(result.is_ok());

        // Send request 2
        let req2 = r#"{"jsonrpc":"2.0","id":2,"method":"tool_result","params":{"tool_call_id":"tc-1","result":"file1.rs\nfile2.rs\n"}}"#;
        client_writer
            .write_all(format!("{req2}\n").as_bytes())
            .unwrap();
        client_writer.flush().unwrap();

        // Read responses for request 2
        let result = super::read_response_loop(&mut client_reader, &stdout);
        assert!(result.is_ok());

        handle.join().unwrap();

        let _ = std::fs::remove_dir_all(&sock_dir);
    }

    #[test]
    fn shim_detects_daemon_disconnect() {
        let sock_dir = std::env::temp_dir().join(format!("tb-dc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        std::fs::create_dir_all(&sock_dir).unwrap();
        let sock_path = sock_dir.join("t.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn a mock daemon that sends a notification then drops the connection
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;

            let mut req = String::new();
            std::io::BufRead::read_line(&mut reader, &mut req).unwrap();

            let notif = r#"{"jsonrpc":"2.0","method":"output","params":{"stream":"content","data":{"type":"text","text":"partial"}}}"#;
            writer.write_all(notif.as_bytes()).unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();

            // Drop writer — daemon disconnects mid-response
            drop(writer);
        });

        let stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let mut client_writer = stream.try_clone().unwrap();
        let mut client_reader = BufReader::new(stream);

        let req = r#"{"jsonrpc":"2.0","id":1,"method":"llm_call","params":{"messages":[{"role":"user","content":"Hello"}],"tools":[]}}"#;
        client_writer
            .write_all(format!("{req}\n").as_bytes())
            .unwrap();
        client_writer.flush().unwrap();

        let stdout = std::io::stdout();
        let result = super::read_response_loop(&mut client_reader, &stdout);
        assert_eq!(result, Err(1));

        handle.join().unwrap();

        let _ = std::fs::remove_dir_all(&sock_dir);
    }
}
