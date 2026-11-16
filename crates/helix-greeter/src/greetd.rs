//! greetd's IPC protocol (helix-desktop-spec `components/greeter.md`, "The
//! greetd protocol"): each message is a 32-bit length in native byte order
//! followed by that many bytes of JSON.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

/// A request to greetd.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    CreateSession { username: String },
    PostAuthMessageResponse {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        response: Option<String>,
    },
    StartSession { cmd: Vec<String>, env: Vec<String> },
    CancelSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// Wrong credentials.
    AuthError,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    /// A question whose answer may be shown.
    Visible,
    /// A question whose answer is hidden (a password).
    Secret,
    Info,
    Error,
}

/// greetd's reply to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Success,
    Error { error_type: ErrorType, description: String },
    AuthMessage { auth_message_type: AuthMessageType, auth_message: String },
}

/// Something that answers greetd requests: greetd itself, or the demo.
pub trait Greetd: Send {
    fn request(&mut self, request: &Request) -> io::Result<Response>;
}

/// Write one length-prefixed JSON message.
pub fn write_message<W: Write, T: Serialize>(w: &mut W, message: &T) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    w.write_all(&len.to_ne_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON message.
pub fn read_message<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_ne_bytes(len) as usize;
    // greetd's messages are small; refuse nonsense rather than allocate it.
    if len > 1 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("greetd message of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A connection to greetd's socket.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connect to the socket `GREETD_SOCK` names.
    pub fn connect_env() -> io::Result<Self> {
        let path = std::env::var_os("GREETD_SOCK").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "GREETD_SOCK is not set"))?;
        Ok(Self { stream: UnixStream::connect(path)? })
    }

    #[cfg(test)]
    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }
}

impl Greetd for Client {
    fn request(&mut self, request: &Request) -> io::Result<Response> {
        write_message(&mut self.stream, request)?;
        read_message(&mut self.stream)
    }
}

/// The greetd stand-in for `--demo`: anyone may log in with the password
/// "demo"; the session it would start is printed instead.
#[derive(Default)]
pub struct Demo {
    user: Option<String>,
    authenticated: bool,
}

pub const DEMO_PASSWORD: &str = "demo";

impl Greetd for Demo {
    fn request(&mut self, request: &Request) -> io::Result<Response> {
        Ok(match request {
            Request::CreateSession { username } => {
                self.user = Some(username.clone());
                self.authenticated = false;
                Response::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: "Password:".into() }
            }
            Request::PostAuthMessageResponse { response } => {
                if self.user.is_none() {
                    return Ok(Response::Error { error_type: ErrorType::Error, description: "no login in progress".into() });
                }
                if response.as_deref() == Some(DEMO_PASSWORD) {
                    self.authenticated = true;
                    Response::Success
                } else {
                    self.user = None;
                    Response::Error { error_type: ErrorType::AuthError, description: "pam_authenticate: AUTH_ERR".into() }
                }
            }
            Request::StartSession { cmd, env } => {
                if !self.authenticated {
                    return Ok(Response::Error { error_type: ErrorType::Error, description: "not authenticated".into() });
                }
                let shown = serde_json::json!({ "user": self.user, "cmd": cmd, "env": env });
                println!("start_session {shown}");
                Response::Success
            }
            Request::CancelSession => {
                self.user = None;
                self.authenticated = false;
                Response::Success
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn wire_format() {
        let json = |r: &Request| serde_json::to_string(r).unwrap();
        assert_eq!(json(&Request::CreateSession { username: "ann".into() }), r#"{"type":"create_session","username":"ann"}"#);
        assert_eq!(json(&Request::PostAuthMessageResponse { response: None }), r#"{"type":"post_auth_message_response"}"#);
        assert_eq!(json(&Request::PostAuthMessageResponse { response: Some("pw".into()) }), r#"{"type":"post_auth_message_response","response":"pw"}"#);
        assert_eq!(json(&Request::CancelSession), r#"{"type":"cancel_session"}"#);
        assert_eq!(
            json(&Request::StartSession { cmd: vec!["/bin/sh".into()], env: vec!["A=b".into()] }),
            r#"{"type":"start_session","cmd":["/bin/sh"],"env":["A=b"]}"#
        );
        let parse = |s: &str| serde_json::from_str::<Response>(s).unwrap();
        assert_eq!(parse(r#"{"type":"success"}"#), Response::Success);
        assert_eq!(
            parse(r#"{"type":"error","error_type":"auth_error","description":"pam_authenticate: AUTH_ERR"}"#),
            Response::Error { error_type: ErrorType::AuthError, description: "pam_authenticate: AUTH_ERR".into() }
        );
        assert_eq!(
            parse(r#"{"type":"auth_message","auth_message_type":"secret","auth_message":"Password: "}"#),
            Response::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: "Password: ".into() }
        );
    }

    #[test]
    fn framing() {
        let mut buf = Vec::new();
        write_message(&mut buf, &Request::CancelSession).unwrap();
        let body = br#"{"type":"cancel_session"}"#;
        assert_eq!(&buf[..4], &(body.len() as u32).to_ne_bytes());
        assert_eq!(&buf[4..], body);
        let back: Request = read_message(&mut buf.as_slice()).unwrap();
        assert_eq!(back, Request::CancelSession);
        // A truncated message is an error, not a hang or a panic.
        assert!(read_message::<_, Request>(&mut &buf[..10]).is_err());
    }

    /// A scripted greetd: for each expected request, the reply to send.
    fn fake_greetd(script: Vec<(Request, Response)>) -> (Client, thread::JoinHandle<()>) {
        let (client, mut server) = UnixStream::pair().unwrap();
        let handle = thread::spawn(move || {
            for (expected, reply) in script {
                let got: Request = read_message(&mut server).unwrap();
                assert_eq!(got, expected);
                write_message(&mut server, &reply).unwrap();
            }
        });
        (Client::from_stream(client), handle)
    }

    fn secret(text: &str) -> Response {
        Response::AuthMessage { auth_message_type: AuthMessageType::Secret, auth_message: text.into() }
    }

    #[test]
    fn login_against_fake_greetd() {
        let start = Request::StartSession { cmd: vec!["helix-session".into()], env: vec!["XDG_SESSION_TYPE=wayland".into()] };
        let (mut client, server) = fake_greetd(vec![
            (Request::CreateSession { username: "ann".into() }, secret("Password: ")),
            (Request::PostAuthMessageResponse { response: Some("pw".into()) }, Response::Success),
            (start.clone(), Response::Success),
        ]);
        assert_eq!(client.request(&Request::CreateSession { username: "ann".into() }).unwrap(), secret("Password: "));
        assert_eq!(client.request(&Request::PostAuthMessageResponse { response: Some("pw".into()) }).unwrap(), Response::Success);
        assert_eq!(client.request(&start).unwrap(), Response::Success);
        server.join().unwrap();
    }

    #[test]
    fn errors_and_cancel_against_fake_greetd() {
        let auth_error = Response::Error { error_type: ErrorType::AuthError, description: "AUTH_ERR".into() };
        let info = Response::AuthMessage { auth_message_type: AuthMessageType::Info, auth_message: "Touch the key".into() };
        let (mut client, server) = fake_greetd(vec![
            (Request::CreateSession { username: "bob".into() }, info.clone()),
            (Request::PostAuthMessageResponse { response: None }, secret("Password: ")),
            (Request::PostAuthMessageResponse { response: Some("wrong".into()) }, auth_error.clone()),
            (Request::CancelSession, Response::Success),
        ]);
        assert_eq!(client.request(&Request::CreateSession { username: "bob".into() }).unwrap(), info);
        assert_eq!(client.request(&Request::PostAuthMessageResponse { response: None }).unwrap(), secret("Password: "));
        assert_eq!(client.request(&Request::PostAuthMessageResponse { response: Some("wrong".into()) }).unwrap(), auth_error);
        assert_eq!(client.request(&Request::CancelSession).unwrap(), Response::Success);
        server.join().unwrap();
        // greetd gone: an error, not a panic.
        assert!(client.request(&Request::CancelSession).is_err());
    }

    #[test]
    fn socket_from_env() {
        let dir = std::env::temp_dir().join(format!("helix-greeter-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("greetd.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _: Request = read_message(&mut stream).unwrap();
            write_message(&mut stream, &Response::Success).unwrap();
        });
        // Only this test sets GREETD_SOCK.
        unsafe { std::env::set_var("GREETD_SOCK", &path) };
        let mut client = Client::connect_env().unwrap();
        assert_eq!(client.request(&Request::CancelSession).unwrap(), Response::Success);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn demo() {
        let mut demo = Demo::default();
        assert!(matches!(demo.request(&Request::CreateSession { username: "x".into() }).unwrap(), Response::AuthMessage { auth_message_type: AuthMessageType::Secret, .. }));
        assert!(matches!(demo.request(&Request::PostAuthMessageResponse { response: Some("nope".into()) }).unwrap(), Response::Error { error_type: ErrorType::AuthError, .. }));
        demo.request(&Request::CreateSession { username: "x".into() }).unwrap();
        assert_eq!(demo.request(&Request::PostAuthMessageResponse { response: Some("demo".into()) }).unwrap(), Response::Success);
        assert_eq!(demo.request(&Request::StartSession { cmd: vec![], env: vec![] }).unwrap(), Response::Success);
    }
}
