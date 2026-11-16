use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::Event;

pub struct Slang {
    child: Child,
    input: ChildStdin,
    search_paths: Vec<String>,
}

impl Slang {
    pub fn start(
        executable: &str,
        folders: &[Value],
        search_paths: Vec<String>,
        events: Sender<Event>,
        generation: u64,
    ) -> io::Result<Self> {
        let mut child = Command::new(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| io::Error::new(err.kind(), format!("starting {executable}: {err}")))?;
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        let mut slang = Self {
            child,
            input,
            search_paths,
        };

        let (initialized, response) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut output = BufReader::new(output);
            loop {
                match read_message(&mut output) {
                    Ok(message) if message.get("id") == Some(&json!(1)) && message.get("method").is_none() => {
                        let _ = initialized.try_send(message);
                    }
                    Ok(message) => {
                        if events.send(Event::Slang(generation, Ok(message))).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = initialized.try_send(json!({ "error": err.to_string() }));
                        let _ = events.send(Event::Slang(generation, Err(err)));
                        break;
                    }
                }
            }
        });

        slang.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": folders.first().and_then(|folder| folder.get("uri")),
                "workspaceFolders": folders,
                "capabilities": {
                    "general": { "positionEncodings": ["utf-16"] },
                    "workspace": { "configuration": true },
                    "textDocument": {
                        "completion": {
                            "completionItem": { "snippetSupport": false, "insertReplaceSupport": true }
                        },
                        "hover": { "contentFormat": ["markdown", "plaintext"] }
                    }
                }
            }
        }))?;
        let response = response
            .recv_timeout(Duration::from_secs(10))
            .map_err(io::Error::other)?;
        if let Some(error) = response.get("error") {
            return Err(io::Error::other(format!("initializing slangd: {error}")));
        }
        if response
            .pointer("/result/capabilities/positionEncoding")
            .and_then(Value::as_str)
            .is_some_and(|encoding| encoding != "utf-16")
        {
            return Err(io::Error::other("slangd must use UTF-16 positions"));
        }
        slang.notify("initialized", json!({}))?;
        // Apply the configuration before the first document, even if slangd's
        // workspace/configuration request is still queued.
        slang.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": { "slang.additionalSearchPaths": slang.search_paths } }),
        )?;
        Ok(slang)
    }

    pub fn open_document(&mut self, uri: &str, text: &str) -> io::Result<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "text": text, "languageId": "slang", "version": 1 }
            }),
        )
    }

    pub fn close_document(&mut self, uri: &str) -> io::Result<()> {
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }))
    }

    pub fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    pub fn request(&mut self, id: u64, method: &str, params: Value) -> io::Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
    }

    pub fn respond(&mut self, request: &Value, id: &Value) -> io::Result<()> {
        let response = match request["method"].as_str() {
            Some("workspace/configuration") => {
                let items = request["params"]["items"].as_array().map_or(&[][..], Vec::as_slice);
                let values: Vec<_> = items
                    .iter()
                    .map(|item| match item["section"].as_str() {
                        Some("slang.additionalSearchPaths") => json!(self.search_paths),
                        _ => Value::Null,
                    })
                    .collect();
                json!({ "result": values })
            }
            Some("client/registerCapability" | "workspace/semanticTokens/refresh" | "workspace/inlayHint/refresh") => {
                json!({ "result": null })
            }
            _ => json!({ "error": { "code": -32601, "message": "Method not supported" } }),
        };
        let mut message = response;
        message["jsonrpc"] = json!("2.0");
        message["id"] = id.clone();
        self.send(message)
    }

    fn send(&mut self, message: Value) -> io::Result<()> {
        let body = serde_json::to_vec(&message)?;
        write!(self.input, "Content-Length: {}\r\n\r\n", body.len())?;
        self.input.write_all(&body)?;
        self.input.flush()
    }
}

impl Drop for Slang {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_message(input: &mut impl BufRead) -> io::Result<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "slangd closed stdout"));
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let length = content_length.ok_or_else(|| io::Error::other("missing LSP Content-Length"))?;
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}
