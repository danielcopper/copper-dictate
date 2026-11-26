use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::{fs, process};

/// A directory under the system's temporary directory, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "cd-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) name: String,
    pub(crate) file_name: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) data: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub(crate) fn multipart(&self) -> Vec<Part> {
        let content_type = self.header("content-type").expect("a content type");
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .expect("a multipart body");
        let delimiter = format!("--{boundary}");
        let body = &self.body;
        let mut parts = Vec::new();
        let mut rest = &body[..];
        assert!(
            rest.starts_with(delimiter.as_bytes()),
            "the body starts with the boundary"
        );
        rest = &rest[delimiter.len()..];
        while !rest.starts_with(b"--") {
            rest = rest
                .strip_prefix(b"\r\n")
                .expect("a line break after the boundary");
            let end =
                find(rest, format!("\r\n{delimiter}").as_bytes()).expect("a closing boundary");
            parts.push(parse_part(&rest[..end]));
            rest = &rest[end + 2 + delimiter.len()..];
        }
        assert_eq!(rest, b"--\r\n", "the body ends after the closing boundary");
        parts
    }
}

fn parse_part(part: &[u8]) -> Part {
    let head_end = find(part, b"\r\n\r\n").expect("part headers");
    let head = std::str::from_utf8(&part[..head_end]).unwrap();
    let mut name = None;
    let mut file_name = None;
    let mut content_type = None;
    for line in head.split("\r\n") {
        let (key, value) = line.split_once(": ").unwrap();
        match key.to_ascii_lowercase().as_str() {
            "content-disposition" => {
                for field in value.split("; ").skip(1) {
                    let (key, quoted) = field.split_once('=').unwrap();
                    let unquoted = quoted.trim_matches('"').to_owned();
                    match key {
                        "name" => name = Some(unquoted),
                        "filename" => file_name = Some(unquoted),
                        other => panic!("unexpected disposition field {other}"),
                    }
                }
            }
            "content-type" => content_type = Some(value.to_owned()),
            other => panic!("unexpected part header {other}"),
        }
    }
    Part {
        name: name.expect("a part name"),
        file_name,
        content_type,
        data: part[head_end + 4..].to_vec(),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// An HTTP server on a local port that answers each request with the next of
/// the given responses and hands the requests it received to the test.
pub(crate) struct FakeHttpServer {
    pub(crate) url: String,
    pub(crate) requests: Receiver<Request>,
}

impl FakeHttpServer {
    pub(crate) fn start(responses: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/audio/transcriptions",
            listener.local_addr().unwrap()
        );
        let (sender, requests) = mpsc::channel();
        thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let request = answer(stream, status, &body);
                if sender.send(request).is_err() {
                    return;
                }
            }
        });
        Self { url, requests }
    }
}

fn answer(stream: TcpStream, status: u16, body: &str) -> Request {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut words = request_line.split_whitespace();
    let method = words.next().unwrap().to_owned();
    let path = words.next().unwrap().to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .map_or(0, |length| length.parse().unwrap());
    let mut request_body = vec![0; length];
    reader.read_exact(&mut request_body).unwrap();

    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream.flush().unwrap();
    Request {
        method,
        path,
        headers,
        body: request_body,
    }
}
