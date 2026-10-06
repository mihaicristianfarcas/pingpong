//! A small HTTP/1.1 server, enough for the mock's web APIs: one thread per
//! connection, keep-alive (the client's agent reuses connections), bodies
//! by `Content-Length`. Plain HTTP on localhost: the client sends every
//! request here when `PING_XBOX_MOCK` says so.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A request, as the handler sees it.
pub struct Request {
    pub method: String,
    /// The path and query, e.g. `/xsts.auth.xboxlive.com/xsts/authorize`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The address the client reached this server at: what the console
    /// tells it to connect to.
    pub local: SocketAddr,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

impl Response {
    pub fn json(v: serde_json::Value) -> Response {
        Response {
            status: 200,
            body: v.to_string(),
        }
    }

    pub fn status(status: u16, body: impl Into<String>) -> Response {
        Response {
            status,
            body: body.into(),
        }
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

pub struct Server {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(bind: SocketAddr, handler: Handler) -> std::io::Result<Server> {
        let listener = TcpListener::bind(bind)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("xbox-mock-http".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let handler = handler.clone();
                                let stop = stop.clone();
                                let _ = std::thread::Builder::new()
                                    .name("xbox-mock-conn".into())
                                    .spawn(move || serve(stream, handler, stop));
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                })?
        };
        Ok(Server {
            addr,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(stream: TcpStream, handler: Handler, stop: Arc<AtomicBool>) {
    let _ = stream.set_nonblocking(false);
    let Ok(local) = stream.local_addr() else {
        return;
    };
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    while !stop.load(Ordering::Relaxed) {
        // Wait for a request to begin in short steps (to see a stop), then
        // read all of it with a timeout that does not cut it in half.
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(200)));
        match reader.fill_buf() {
            Ok([]) => return,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return,
        }
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(5)));
        let request = match read_request(&mut reader, local) {
            Ok(Some(r)) => r,
            _ => return,
        };
        let response = handler(&request);
        let reason = match response.status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Status",
        };
        let head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: keep-alive\r\n\r\n",
            response.status,
            response.body.len()
        );
        if writer.write_all(head.as_bytes()).is_err()
            || writer.write_all(response.body.as_bytes()).is_err()
        {
            return;
        }
    }
}

/// One request, or `None` at the end of the connection.
fn read_request(
    reader: &mut BufReader<TcpStream>,
    local: SocketAddr,
) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, path) = (method.to_owned(), path.to_owned());
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            return Ok(None);
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len.min(16 << 20)];
    reader.read_exact(&mut body)?;
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
        local,
    }))
}
