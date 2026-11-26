use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, process};

use crate::config::{ApiKey, FinalServerConfig};
use crate::session::{AudioSource, BoxError, Capture, FinalServer, Recording, Stopped};

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

pub(crate) struct FakeHttpServer {
    pub(crate) url: String,
    pub(crate) requests: Receiver<Request>,
}

pub(crate) struct HttpAnswer {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: Vec<u8>,
}

impl FakeHttpServer {
    pub(crate) fn start(responses: Vec<(u16, String)>) -> Self {
        Self::answering(
            responses
                .into_iter()
                .map(|(status, body)| HttpAnswer {
                    status,
                    headers: Vec::new(),
                    body: body.into_bytes(),
                })
                .collect(),
        )
    }

    pub(crate) fn answering(answers: Vec<HttpAnswer>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/audio/transcriptions",
            listener.local_addr().unwrap()
        );
        let (sender, requests) = mpsc::channel();
        thread::spawn(move || {
            for reply in answers {
                let (stream, _) = listener.accept().unwrap();
                let request = answer(stream, &reply);
                if sender.send(request).is_err() {
                    return;
                }
            }
        });
        Self { url, requests }
    }
}

fn answer(stream: TcpStream, reply: &HttpAnswer) -> Request {
    let mut reader = BufReader::new(stream);
    let request = read_request(&mut reader);
    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    )
    .unwrap();
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    stream.write_all(&reply.body).unwrap();
    stream.flush().unwrap();
    request
}

pub(crate) fn read_request(reader: &mut impl BufRead) -> Request {
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
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        method,
        path,
        headers,
        body,
    }
}

pub(crate) struct FakeMicrophone {
    stopped: Stopped,
    events: Sender<&'static str>,
}

impl FakeMicrophone {
    pub(crate) fn new(samples: Vec<i16>) -> (Self, Receiver<&'static str>) {
        Self::glitching(samples, 0)
    }

    pub(crate) fn glitching(samples: Vec<i16>, glitches: u32) -> (Self, Receiver<&'static str>) {
        let (events, received) = mpsc::channel();
        let stopped = Stopped {
            recording: Recording { samples },
            glitches,
        };
        (Self { stopped, events }, received)
    }
}

impl AudioSource for FakeMicrophone {
    fn start(&self) -> Result<Box<dyn Capture>, BoxError> {
        self.events.send("start")?;
        Ok(Box::new(FakeCapture {
            stopped: self.stopped.clone(),
            events: self.events.clone(),
        }))
    }
}

struct FakeCapture {
    stopped: Stopped,
    events: Sender<&'static str>,
}

impl Capture for FakeCapture {
    fn stop(self: Box<Self>) -> Result<Stopped, BoxError> {
        self.events.send("stop")?;
        Ok(self.stopped)
    }
}

pub(crate) struct MicrophoneThatStops;

impl AudioSource for MicrophoneThatStops {
    fn start(&self) -> Result<Box<dyn Capture>, BoxError> {
        Ok(Box::new(MicrophoneThatStops))
    }
}

impl Capture for MicrophoneThatStops {
    fn stop(self: Box<Self>) -> Result<Stopped, BoxError> {
        Err("the device is gone".into())
    }
}

pub(crate) struct BrokenMicrophone;

impl AudioSource for BrokenMicrophone {
    fn start(&self) -> Result<Box<dyn Capture>, BoxError> {
        Err("no input device".into())
    }
}

pub(crate) fn final_server_config(url: &str, api_key: Option<&str>) -> FinalServerConfig {
    FinalServerConfig {
        endpoint: url.parse().unwrap(),
        model: "whisper-large-v3".to_owned(),
        api_key: api_key.map(|key| ApiKey::parse(key.as_bytes()).unwrap()),
    }
}

pub(crate) struct FakeFinalServer {
    answers: Mutex<Vec<Result<String, String>>>,
    pub(crate) received: Arc<Mutex<Vec<Recording>>>,
    release: Option<Mutex<Receiver<()>>>,
}

impl FakeFinalServer {
    pub(crate) fn answering(answers: &[Result<&str, &str>]) -> Self {
        Self {
            answers: Mutex::new(
                answers
                    .iter()
                    .rev()
                    .map(|answer| answer.map(str::to_owned).map_err(str::to_owned))
                    .collect(),
            ),
            received: Arc::default(),
            release: None,
        }
    }

    pub(crate) fn held(mut self) -> (Self, Sender<()>) {
        let (release, wait) = mpsc::channel();
        self.release = Some(Mutex::new(wait));
        (self, release)
    }
}

impl FinalServer for FakeFinalServer {
    fn transcribe(&self, recording: &Recording) -> Result<String, BoxError> {
        self.received.lock().unwrap().push(recording.clone());
        if let Some(release) = &self.release {
            release.lock().unwrap().recv()?;
        }
        let answer = self.answers.lock().unwrap().pop().expect("an answer left");
        answer.map_err(Into::into)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Decoded {
    pub(crate) format: u16,
    pub(crate) channels: u16,
    pub(crate) sample_rate: u32,
    pub(crate) bits_per_sample: u16,
    pub(crate) samples: Vec<i16>,
}

pub(crate) fn decode(wav: &[u8]) -> Decoded {
    let u16_at = |at: usize| u16::from_le_bytes([wav[at], wav[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes(wav[at..at + 4].try_into().unwrap());
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(u32_at(4) as usize, wav.len() - 8);
    assert_eq!(&wav[8..16], b"WAVEfmt ");
    assert_eq!(u32_at(16), 16);
    assert_eq!(&wav[36..40], b"data");
    assert_eq!(u32_at(40) as usize, wav.len() - 44);
    let channels = u16_at(22);
    let sample_rate = u32_at(24);
    let bits_per_sample = u16_at(34);
    assert_eq!(
        u32_at(28),
        sample_rate * u32::from(channels * bits_per_sample / 8)
    );
    assert_eq!(u16_at(32), channels * bits_per_sample / 8);
    Decoded {
        format: u16_at(20),
        channels,
        sample_rate,
        bits_per_sample,
        samples: wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair))
            .collect(),
    }
}

pub(crate) fn wait_until(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Every `.rs` file under `src/`, by its path relative to `src/`, with its content.
pub(crate) fn source_files() -> BTreeMap<String, String> {
    fn collect(dir: &Path, src: &Path, files: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, src, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let name = path.strip_prefix(src).unwrap().display().to_string();
                files.insert(name, fs::read_to_string(&path).unwrap());
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = BTreeMap::new();
    collect(&src, &src, &mut files);
    files
}
