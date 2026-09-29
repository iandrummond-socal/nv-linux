//! Blocking Dropbox HTTP client and unauthenticated longpoll notifier.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{agent, oauth, transport, Endpoints, Response};
use crate::remote::{
    is_note_name, Cursor, Notifier, Remote, RemoteChange, RemoteError, RemoteFile, RemoteResult,
};
use crate::secret::Secret;

pub const MAX_UPLOAD: u64 = 150 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct Account {
    pub account_id: String,
    pub email: Option<String>,
}

pub struct DropboxRemote {
    agent: ureq::Agent,
    ep: Endpoints,
    app_key: String,
    refresh_token: Secret,
    access: Option<(Secret, Instant)>,
}

impl std::fmt::Debug for DropboxRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropboxRemote").finish_non_exhaustive()
    }
}

impl DropboxRemote {
    pub fn new(app_key: impl Into<String>, refresh_token: Secret) -> Self {
        Self::with_endpoints(app_key, refresh_token, &Endpoints::dropbox())
    }

    pub fn with_endpoints(
        app_key: impl Into<String>,
        refresh_token: Secret,
        ep: &Endpoints,
    ) -> Self {
        Self {
            agent: agent(Duration::from_secs(120)),
            ep: ep.clone(),
            app_key: app_key.into(),
            refresh_token,
            access: None,
        }
    }

    fn refresh_access(&mut self) -> RemoteResult<()> {
        self.access = None;
        let token = oauth::refresh(&self.agent, &self.ep, &self.app_key, &self.refresh_token)?;
        let expires = Instant::now()
            .checked_add(Duration::from_secs(token.expires_in))
            .ok_or_else(|| RemoteError::Other("Invalid token expiration".into()))?;
        if let Some(refresh) = token.refresh_token {
            self.refresh_token = refresh;
        }
        self.access = Some((token.access_token, expires));
        Ok(())
    }

    fn call(
        &mut self,
        build: impl Fn(&ureq::Agent, &Secret) -> Result<Response, ureq::Error>,
    ) -> RemoteResult<Response> {
        if self.access.as_ref().is_none_or(|(_, expires)| {
            expires.saturating_duration_since(Instant::now()) <= Duration::from_secs(60)
        }) {
            self.refresh_access()?;
        }
        for attempt in 0..2 {
            let token = &self.access.as_ref().expect("refreshed access token").0;
            let response = build(&self.agent, token).map_err(transport)?;
            if response.status().as_u16() == 401 {
                self.access = None;
                if attempt == 0 {
                    self.refresh_access()?;
                    continue;
                }
                return Err(RemoteError::Unauthorized);
            }
            return check_response(response);
        }
        unreachable!("two attempts always return")
    }

    fn rpc(&mut self, route: &str, body: &Value) -> RemoteResult<Response> {
        let url = format!("{}/2/{route}", self.ep.api);
        self.call(|agent, token| {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", token.expose()))
                .send_json(body)
        })
    }

    pub fn current_account(&mut self) -> RemoteResult<Account> {
        decode(self.rpc("users/get_current_account", &Value::Null)?)
    }

    pub fn revoke(&mut self) -> RemoteResult<()> {
        let url = format!("{}/2/auth/token/revoke", self.ep.api);
        self.call(|agent, token| {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", token.expose()))
                .send_empty()
        })?;
        self.access = None;
        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(mut response: Response) -> RemoteResult<T> {
    let body = response.body_mut().read_to_string().map_err(transport)?;
    serde_json::from_str(&body).map_err(|_| RemoteError::Other("Invalid Dropbox response".into()))
}

pub(crate) fn check_response(mut response: Response) -> RemoteResult<Response> {
    let status = response.status().as_u16();
    match status {
        200..=299 => Ok(response),
        401 => Err(RemoteError::Unauthorized),
        429 => {
            let header = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let seconds = header
                .or_else(|| {
                    response
                        .body_mut()
                        .read_json::<Value>()
                        .ok()
                        .and_then(|v| v["error"]["retry_after"].as_u64())
                })
                .unwrap_or(5);
            Err(RemoteError::RateLimited(Duration::from_secs(seconds)))
        }
        409 => {
            let value = response.body_mut().read_json::<Value>().ok();
            let summary = value
                .as_ref()
                .and_then(|v| v["error_summary"].as_str())
                .unwrap_or("HTTP 409");
            Err(if summary.contains("reset") {
                RemoteError::ResetCursor
            } else if summary.contains("conflict") {
                RemoteError::Conflict
            } else if ["malformed_path", "disallowed_name", "restricted_content"]
                .iter()
                .any(|s| summary.contains(s))
            {
                RemoteError::BadName
            } else if summary.contains("too_large") {
                RemoteError::TooLarge
            } else {
                RemoteError::Other(summary.into())
            })
        }
        500..=599 => Err(RemoteError::Offline),
        _ => Err(RemoteError::Other(format!("HTTP {status}"))),
    }
}

#[derive(Deserialize)]
struct FileMetadata {
    id: String,
    name: String,
    path_lower: String,
    rev: String,
    content_hash: String,
    size: u64,
}

impl From<FileMetadata> for RemoteFile {
    fn from(file: FileMetadata) -> Self {
        Self {
            id: file.id,
            name: file.name,
            key: file.path_lower,
            rev: file.rev,
            content_hash: file.content_hash,
            size: file.size,
        }
    }
}

#[derive(Deserialize)]
struct Page {
    entries: Vec<Value>,
    cursor: String,
    has_more: bool,
}

fn file(value: Value) -> RemoteResult<RemoteFile> {
    serde_json::from_value::<FileMetadata>(value)
        .map(Into::into)
        .map_err(|_| RemoteError::Other("Invalid file metadata".into()))
}

impl Remote for DropboxRemote {
    fn list(&mut self) -> RemoteResult<(Vec<RemoteFile>, Cursor)> {
        let mut page: Page = decode(self.rpc(
            "files/list_folder",
            &json!({
                "path": "", "recursive": false, "include_deleted": false,
            }),
        )?)?;
        let mut files = Vec::new();
        loop {
            for entry in page.entries {
                if entry[".tag"] == "file" && entry["name"].as_str().is_some_and(is_note_name) {
                    files.push(file(entry)?);
                }
            }
            if !page.has_more {
                return Ok((files, Cursor(page.cursor)));
            }
            page = decode(self.rpc(
                "files/list_folder/continue",
                &json!({"cursor": page.cursor}),
            )?)?;
        }
    }

    fn changes(&mut self, since: &Cursor) -> RemoteResult<(Vec<RemoteChange>, Cursor)> {
        let mut cursor = since.0.clone();
        let mut changes = Vec::new();
        loop {
            let page: Page = decode(
                self.rpc("files/list_folder/continue", &json!({"cursor": cursor}))
                    .map_err(|e| match e {
                        RemoteError::Other(m) if m == "HTTP 400" => RemoteError::ResetCursor,
                        e => e,
                    })?,
            )?;
            for entry in page.entries {
                if !entry["name"].as_str().is_some_and(is_note_name) {
                    continue;
                }
                match entry[".tag"].as_str() {
                    Some("file") => changes.push(RemoteChange::Upsert(file(entry)?)),
                    Some("deleted") => {
                        let key = entry["path_lower"]
                            .as_str()
                            .ok_or_else(|| RemoteError::Other("Invalid deleted metadata".into()))?;
                        changes.push(RemoteChange::Deleted { key: key.into() });
                    }
                    _ => {}
                }
            }
            cursor = page.cursor;
            if !page.has_more {
                return Ok((changes, Cursor(cursor)));
            }
        }
    }

    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>> {
        let url = format!("{}/2/files/download", self.ep.content);
        let arg = header_json(&json!({"path": id}));
        let mut response = self.call(|agent, token| {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", token.expose()))
                .header("Dropbox-API-Arg", &arg)
                .send_empty()
        })?;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_UPLOAD + 1)
            .read_to_vec()
            .map_err(transport)?;
        if bytes.len() as u64 >= MAX_UPLOAD {
            return Err(RemoteError::TooLarge);
        }
        Ok(bytes)
    }

    fn upload(
        &mut self,
        name: &str,
        bytes: &[u8],
        expect_rev: Option<&str>,
    ) -> RemoteResult<RemoteFile> {
        if bytes.len() as u64 >= MAX_UPLOAD {
            return Err(RemoteError::TooLarge);
        }
        if !is_note_name(name) {
            return Err(RemoteError::BadName);
        }
        let mode = expect_rev.map_or_else(
            || json!("add"),
            |rev| json!({".tag": "update", "update": rev}),
        );
        let arg = header_json(&json!({"path": format!("/{name}"), "mode": mode,
            "autorename": false, "mute": true, "strict_conflict": true}));
        let url = format!("{}/2/files/upload", self.ep.content);
        let metadata: FileMetadata = decode(self.call(|agent, token| {
            agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", token.expose()))
                .header("Content-Type", "application/octet-stream")
                .header("Dropbox-API-Arg", &arg)
                .send(bytes)
        })?)?;
        Ok(metadata.into())
    }

    fn delete(&mut self, id: &str, parent_rev: &str) -> RemoteResult<()> {
        self.rpc(
            "files/delete_v2",
            &json!({"path": id, "parent_rev": parent_rev}),
        )?;
        Ok(())
    }
}

/// Encode JSON for HTTP headers, including UTF-16 pairs for non-BMP characters.
fn header_json(value: &Value) -> String {
    use std::fmt::Write;
    let json = serde_json::to_string(value).expect("Value serializes");
    let mut result = String::new();
    for ch in json.chars() {
        if ch.is_ascii() {
            result.push(ch);
        } else {
            let mut units = [0; 2];
            for unit in ch.encode_utf16(&mut units) {
                write!(result, "\\u{unit:04x}").expect("writing to String");
            }
        }
    }
    result
}

pub struct DropboxNotifier {
    agent: ureq::Agent,
    ep: Endpoints,
}

impl Default for DropboxNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl DropboxNotifier {
    pub fn new() -> Self {
        Self::with_endpoints(&Endpoints::dropbox())
    }

    pub fn with_endpoints(ep: &Endpoints) -> Self {
        Self {
            agent: agent(Duration::from_secs(120)),
            ep: ep.clone(),
        }
    }
}

impl Notifier for DropboxNotifier {
    /// Honors server backoff by sleeping before returning, even when changes exist.
    fn wait(&mut self, since: &Cursor, timeout: Duration) -> RemoteResult<bool> {
        #[derive(Deserialize)]
        struct Longpoll {
            changes: bool,
            backoff: Option<u64>,
        }
        let seconds = timeout.as_secs().clamp(30, 480);
        self.agent = agent(Duration::from_secs(seconds + 30));
        let response = self
            .agent
            .post(format!("{}/2/files/list_folder/longpoll", self.ep.notify))
            .send_json(json!({"cursor": since.0, "timeout": seconds}))
            .map_err(transport)?;
        let reply: Longpoll = decode(check_response(response).map_err(|e| match e {
            RemoteError::Other(m) if m == "HTTP 400" => RemoteError::ResetCursor,
            e => e,
        })?)?;
        if let Some(seconds) = reply.backoff {
            std::thread::sleep(Duration::from_secs(seconds));
        }
        Ok(reply.changes)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    use super::*;

    const ACCESS: &str = "testAccessSecret928";
    const REFRESH: &str = "testRefreshSecret729";

    struct Request {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    impl Request {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    struct Reply {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: Vec<u8>,
    }

    fn reply(status: u16, value: Value) -> Reply {
        Reply {
            status,
            headers: vec![],
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    fn token() -> Reply {
        reply(200, json!({"access_token": ACCESS, "expires_in": 3600}))
    }

    fn metadata(name: &str) -> Value {
        json!({".tag": "file", "id": "id:one", "name": name,
            "path_lower": "/api-verbatim.txt", "rev": "rev1", "content_hash": "hash", "size": 4})
    }

    fn page(entries: Vec<Value>, cursor: &str, has_more: bool) -> Reply {
        reply(
            200,
            json!({"entries": entries, "cursor": cursor, "has_more": has_more}),
        )
    }

    struct Server {
        ep: Endpoints,
        thread: JoinHandle<Vec<Request>>,
    }

    impl Server {
        fn new(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let root = format!("http://{}", listener.local_addr().unwrap());
            let ep = Endpoints {
                authorize: format!("{root}/authorize"),
                api: root.clone(),
                content: root.clone(),
                notify: root,
            };
            let thread = thread::spawn(move || {
                let mut requests = Vec::new();
                for reply in replies {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(Instant::now() < deadline, "missing mock request");
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(e) => panic!("accept: {e}"),
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).unwrap();
                        head.push(byte[0]);
                        assert!(head.len() < 65536);
                    }
                    let head = String::from_utf8(head).unwrap();
                    let mut lines = head.split("\r\n");
                    let first: Vec<_> = lines.next().unwrap().split_whitespace().collect();
                    let headers: HashMap<_, _> = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
                        .collect();
                    let length = headers
                        .get("content-length")
                        .map_or(0, |v| v.parse::<usize>().unwrap());
                    assert!(
                        !headers.contains_key("transfer-encoding"),
                        "mock expects Content-Length"
                    );
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).unwrap();
                    requests.push(Request {
                        method: first[0].into(),
                        path: first[1].into(),
                        headers,
                        body,
                    });
                    write!(
                        stream,
                        "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    )
                    .unwrap();
                    for (name, value) in reply.headers {
                        write!(stream, "{name}: {value}\r\n").unwrap();
                    }
                    stream.write_all(b"\r\n").unwrap();
                    stream.write_all(&reply.body).unwrap();
                }
                requests
            });
            Self { ep, thread }
        }

        fn remote(&self) -> DropboxRemote {
            DropboxRemote::with_endpoints("app", Secret::new(REFRESH), &self.ep)
        }

        fn finish(self) -> Vec<Request> {
            let requests = self.thread.join().unwrap();
            // Sweep every recorded request, including OAuth and unauthenticated calls.
            for request in &requests {
                assert_eq!(request.method, "POST");
                assert!(!request.path.contains(ACCESS));
                assert!(!request.path.contains(REFRESH));
                for (key, value) in &request.headers {
                    assert!(!value.contains(REFRESH));
                    if value.contains(ACCESS) {
                        assert_eq!(key, "authorization");
                        assert_eq!(value, &format!("Bearer {ACCESS}"));
                    }
                }
                let body = String::from_utf8_lossy(&request.body);
                assert!(!body.contains(ACCESS));
                if body.contains(REFRESH) {
                    assert_eq!(request.path, "/oauth2/token");
                }
            }
            requests
        }
    }

    #[test]
    fn header_unicode_roundtrip() {
        let value = json!({"path": "Café 🎉.txt"});
        let encoded = header_json(&value);
        assert!(encoded.is_ascii());
        assert!(encoded.contains("\\u00e9"));
        assert!(encoded.contains("\\ud83c\\udf89"));
        assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), value);
    }

    #[test]
    fn list_and_changes_page_and_filter() {
        let server = Server::new(vec![
            token(),
            page(
                vec![
                    metadata("a.txt"),
                    metadata("image.jpg"),
                    json!({".tag":"folder", "name":"folder.txt"}),
                ],
                "c1",
                true,
            ),
            page(vec![metadata("b.md")], "c2", false),
            page(
                vec![
                    json!({".tag":"deleted", "name":"gone.txt", "path_lower":"/gone.txt"}),
                    json!({".tag":"folder", "name":"folder.txt"}),
                    metadata("image.jpg"),
                    json!({".tag":"deleted", "name":"image.jpg", "path_lower":"/image.jpg"}),
                ],
                "c3",
                true,
            ),
            page(vec![metadata("new.txt")], "c4", false),
        ]);
        let mut remote = server.remote();
        let (files, cursor) = remote.list().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].key, "/api-verbatim.txt");
        assert_eq!(cursor, Cursor("c2".into()));
        let (changes, cursor) = remote.changes(&cursor).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(
            changes[0],
            RemoteChange::Deleted {
                key: "/gone.txt".into()
            }
        );
        assert!(matches!(&changes[1], RemoteChange::Upsert(f) if f.name == "new.txt"));
        assert_eq!(cursor, Cursor("c4".into()));
        let requests = server.finish();
        assert_eq!(requests[1].path, "/2/files/list_folder");
        assert_eq!(
            requests[1].json(),
            json!({"path":"", "recursive":false, "include_deleted":false})
        );
        for (request, cursor) in requests[2..].iter().zip(["c1", "c2", "c3"]) {
            assert_eq!(request.path, "/2/files/list_folder/continue");
            assert_eq!(request.json(), json!({"cursor":cursor}));
        }
    }

    #[test]
    fn changes_rejected_cursor_resets() {
        let server = Server::new(vec![token(), reply(400, json!({"error":"bad cursor"}))]);
        assert_eq!(
            server.remote().changes(&Cursor("garbage".into())),
            Err(RemoteError::ResetCursor)
        );
        let requests = server.finish();
        assert_eq!(requests[1].path, "/2/files/list_folder/continue");
        assert_eq!(requests[1].json(), json!({"cursor":"garbage"}));
    }

    #[test]
    fn content_requests_and_revision_checks() {
        let server = Server::new(vec![
            token(),
            reply(200, metadata("Café 🎉.txt")),
            reply(200, metadata("a.txt")),
            Reply {
                status: 200,
                headers: vec![],
                body: b"note\0bytes".to_vec(),
            },
            reply(200, json!({})),
        ]);
        let mut remote = server.remote();
        remote.upload("Café 🎉.txt", b"text", None).unwrap();
        remote.upload("a.txt", b"edit", Some("old-rev")).unwrap();
        assert_eq!(remote.download("id:one").unwrap(), b"note\0bytes");
        remote.delete("id:one", "parent-rev").unwrap();
        let debug = format!("{remote:?}");
        assert!(!debug.contains(ACCESS));
        assert!(!debug.contains(REFRESH));
        let requests = server.finish();
        let raw = &requests[1].headers["dropbox-api-arg"];
        assert!(raw.is_ascii());
        assert!(raw.contains("\\u00e9"));
        assert!(raw.contains("\\ud83c\\udf89"));
        assert_eq!(
            serde_json::from_str::<Value>(raw).unwrap(),
            json!({"path":"/Café 🎉.txt", "mode":"add", "autorename":false, "mute":true, "strict_conflict":true})
        );
        let arg: Value = serde_json::from_str(&requests[2].headers["dropbox-api-arg"]).unwrap();
        assert_eq!(arg["mode"], json!({".tag":"update", "update":"old-rev"}));
        assert_eq!(requests[1].path, "/2/files/upload");
        assert_eq!(
            requests[1].headers["content-type"],
            "application/octet-stream"
        );
        assert_eq!(requests[1].body, b"text");
        assert_eq!(requests[2].body, b"edit");
        assert_eq!(requests[3].path, "/2/files/download");
        assert_eq!(
            serde_json::from_str::<Value>(&requests[3].headers["dropbox-api-arg"]).unwrap(),
            json!({"path":"id:one"})
        );
        assert!(requests[3].body.is_empty());
        assert_eq!(requests[4].path, "/2/files/delete_v2");
        assert_eq!(
            requests[4].json(),
            json!({"path":"id:one", "parent_rev":"parent-rev"})
        );
    }

    #[test]
    fn unauthorized_refreshes_once() {
        for final_status in [200, 401] {
            let server = Server::new(vec![
                token(),
                reply(401, json!({})),
                token(),
                reply(
                    final_status,
                    json!({"account_id":"account", "email":"a@example.org"}),
                ),
            ]);
            let mut remote = server.remote();
            let result = remote.current_account();
            if final_status == 200 {
                assert_eq!(result.unwrap().account_id, "account");
            } else {
                assert!(matches!(result, Err(RemoteError::Unauthorized)));
            }
            let requests = server.finish();
            assert_eq!(
                requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
                [
                    "/oauth2/token",
                    "/2/users/get_current_account",
                    "/oauth2/token",
                    "/2/users/get_current_account"
                ]
            );
            assert_eq!(requests[1].json(), Value::Null);
        }
    }

    #[test]
    fn error_mapping() {
        let mut limited = reply(429, json!({"error":{"retry_after":99}}));
        limited.headers.push(("Retry-After", "7"));
        let cases = vec![
            (limited, RemoteError::RateLimited(Duration::from_secs(7))),
            (
                reply(429, json!({"error":{"retry_after":8}})),
                RemoteError::RateLimited(Duration::from_secs(8)),
            ),
            (
                reply(429, json!({})),
                RemoteError::RateLimited(Duration::from_secs(5)),
            ),
            (
                reply(409, json!({"error_summary":"path/conflict/file/.."})),
                RemoteError::Conflict,
            ),
            (
                reply(409, json!({"error_summary":"reset/.."})),
                RemoteError::ResetCursor,
            ),
            (
                reply(409, json!({"error_summary":"path/malformed_path/.."})),
                RemoteError::BadName,
            ),
            (
                reply(409, json!({"error_summary":"too_large/.."})),
                RemoteError::TooLarge,
            ),
            (reply(400, json!({})), RemoteError::Other("HTTP 400".into())),
            (reply(503, json!({})), RemoteError::Offline),
            (reply(403, json!({})), RemoteError::Other("HTTP 403".into())),
        ];
        for (response, expected) in cases {
            let server = Server::new(vec![token(), response]);
            assert_eq!(server.remote().list().unwrap_err(), expected);
            server.finish();
        }
    }

    #[test]
    fn invalid_uploads_do_not_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let ep = Endpoints {
            authorize: root.clone(),
            api: root.clone(),
            content: root.clone(),
            notify: root,
        };
        let mut remote = DropboxRemote::with_endpoints("app", Secret::new(REFRESH), &ep);
        assert_eq!(
            remote.upload("image.jpg", b"", None),
            Err(RemoteError::BadName)
        );
        let bytes = vec![0; MAX_UPLOAD as usize];
        assert_eq!(
            remote.upload("a.txt", &bytes, None),
            Err(RemoteError::TooLarge)
        );
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn oauth_forms_and_redaction() {
        let server = Server::new(vec![
            reply(
                200,
                json!({"access_token":ACCESS, "refresh_token":REFRESH,
            "expires_in":3600, "account_id":"account"}),
            ),
            token(),
            reply(200, json!({})),
        ]);
        let agent = agent(Duration::from_secs(5));
        let tokens = oauth::exchange_code(
            &agent,
            &server.ep,
            "app key",
            &Secret::new("  pasted+code\n"),
            &Secret::new("verifier"),
        )
        .unwrap();
        assert_eq!(tokens.access_token.expose(), ACCESS);
        let debug = format!("{tokens:?}");
        assert!(!debug.contains(ACCESS));
        assert!(!debug.contains(REFRESH));
        oauth::refresh(
            &agent,
            &server.ep,
            "app",
            tokens.refresh_token.as_ref().unwrap(),
        )
        .unwrap();
        oauth::revoke(&agent, &server.ep, &tokens.access_token).unwrap();
        let requests = server.finish();
        let fields: HashMap<_, _> = std::str::from_utf8(&requests[0].body)
            .unwrap()
            .split('&')
            .map(|field| field.split_once('=').unwrap())
            .collect();
        assert_eq!(requests[0].path, "/oauth2/token");
        assert!(
            requests[0].headers["content-type"].starts_with("application/x-www-form-urlencoded")
        );
        assert_eq!(fields["code"], "pasted%2Bcode");
        assert_eq!(fields["grant_type"], "authorization_code");
        assert_eq!(fields["code_verifier"], "verifier");
        assert_eq!(fields["client_id"], "app+key");
        assert!(!fields.contains_key("redirect_uri"));
        assert_eq!(
            String::from_utf8_lossy(&requests[1].body),
            format!("grant_type=refresh_token&refresh_token={REFRESH}&client_id=app")
        );
        assert_eq!(requests[2].path, "/2/auth/token/revoke");
        assert!(requests[2].body.is_empty());
    }

    #[test]
    fn oauth_rejects_invalid_grants() {
        for status in [400, 401] {
            let server = Server::new(vec![reply(
                status,
                json!({"error":"invalid_grant", "error_description":ACCESS}),
            )]);
            let result = oauth::refresh(
                &agent(Duration::from_secs(5)),
                &server.ep,
                "app",
                &Secret::new(REFRESH),
            );
            assert!(matches!(result, Err(RemoteError::Unauthorized)));
            server.finish();
        }
        let server = Server::new(vec![reply(400, json!({"error":"invalid_grant"}))]);
        let result = oauth::exchange_code(
            &agent(Duration::from_secs(5)),
            &server.ep,
            "app",
            &Secret::new("code"),
            &Secret::new("verifier"),
        );
        assert!(matches!(result, Err(RemoteError::Unauthorized)));
        server.finish();
    }

    #[test]
    fn longpoll_rejected_cursor_resets() {
        let server = Server::new(vec![reply(400, json!({"error":"bad cursor"}))]);
        let mut notifier = DropboxNotifier::with_endpoints(&server.ep);
        assert_eq!(
            notifier.wait(&Cursor("garbage".into()), Duration::from_secs(30)),
            Err(RemoteError::ResetCursor)
        );
        let requests = server.finish();
        assert_eq!(requests[0].path, "/2/files/list_folder/longpoll");
        assert_eq!(
            requests[0].json(),
            json!({"cursor":"garbage", "timeout":30})
        );
    }

    #[test]
    fn longpoll_is_unauthenticated_and_clamped() {
        for (requested, expected) in [(0, 30), (900, 480)] {
            let server = Server::new(vec![
                reply(200, json!({"changes":true, "backoff":0})),
                reply(409, json!({"error_summary":"reset/.."})),
            ]);
            let mut notifier = DropboxNotifier::with_endpoints(&server.ep);
            assert!(notifier
                .wait(&Cursor("cursor".into()), Duration::from_secs(requested))
                .unwrap());
            assert_eq!(
                notifier.wait(&Cursor("cursor".into()), Duration::from_secs(30)),
                Err(RemoteError::ResetCursor)
            );
            let requests = server.finish();
            assert_eq!(requests[0].path, "/2/files/list_folder/longpoll");
            assert_eq!(
                requests[0].json(),
                json!({"cursor":"cursor", "timeout":expected})
            );
            assert!(requests
                .iter()
                .all(|r| !r.headers.contains_key("authorization")));
        }
    }

    #[test]
    fn download_exceeds_default_body_limit() {
        let bytes = vec![b'x'; 11 * 1024 * 1024];
        let server = Server::new(vec![
            token(),
            Reply {
                status: 200,
                headers: vec![],
                body: bytes.clone(),
            },
        ]);
        assert_eq!(server.remote().download("id:one").unwrap(), bytes);
        server.finish();
    }

    #[test]
    fn expiring_access_is_refreshed_and_revoke_clears_access() {
        let server = Server::new(vec![token(), reply(200, json!({}))]);
        let mut remote = server.remote();
        remote.access = Some((
            Secret::new("expiredAccess"),
            Instant::now() + Duration::from_secs(30),
        ));
        remote.revoke().unwrap();
        assert!(remote.access.is_none());
        let requests = server.finish();
        assert_eq!(requests[0].path, "/oauth2/token");
        assert_eq!(requests[1].path, "/2/auth/token/revoke");
        assert!(requests[1].body.is_empty());
    }

    #[test]
    fn clients_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DropboxRemote>();
        assert_send::<DropboxNotifier>();
    }
}
