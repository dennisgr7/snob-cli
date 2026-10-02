//! What a command and the owner of the browsers say to each other.
//!
//! **snob's own requests, never the browser's protocol.** The owner is handed
//! a session and a request, or an intent the allowlist names
//! (`snob_ig::web::Ask`), and hands back an answer; it checks the intent
//! again itself, and builds the request for it beside the page's values.
//! Nothing that reaches the socket can ask the browser for anything else. The
//! debugging protocol is the one thing that must never be reachable from
//! outside the process that started the browser — a debugging port hands the
//! session cookie to any local process that asks (`pipe.rs`) — and a socket
//! that relayed it would be that port again.
//!
//! Each message is its length, four bytes big-endian, and that many bytes of
//! JSON. A length past [`MAX_MESSAGE_BYTES`] ends the connection: nothing either
//! side sends comes near it, so one that claims to is not worth reading. The
//! buffers a message is built and read in are allocated once, at their full
//! size, and wiped when they go, since the request carries the session.
//!
//! **`Hello`, `Welcome`, `Retire`, `Release` and `Done` keep their shape for
//! good.** They are what two builds say to each other: an owner that cannot
//! read a command's `Hello` closes without a `Welcome`, so the command can
//! neither use it nor ask it to leave, and a logout or a purge closes the
//! browsers of whichever build's owner is running. A field may be added to
//! them, with `#[serde(default)]`; none is ever removed or renamed.
//! Everything else is only ever read by the build that wrote it ([`build`]),
//! and a command skips a message from the owner it cannot read: an owner of
//! another build tells every command what it heard, in its own words.

use serde::{Deserialize, Serialize};
use snob_core::Pk;
use snob_core::session::Session;
use snob_ig::client::page::{PageError, PageRequest, PageResponse, PushedBack};
use snob_ig::login::BrowserCookies;
use snob_ig::web::{Call, Told};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// Ceiling on one message: well above the largest page answer a request may
/// ask for (`headless::PAGE_WIRE_CAP`) once it is written as JSON again.
const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Which binary this is. A command sends only through an owner of its own
/// build. An owner of an older build that serves nobody else is asked to
/// leave (`ToOwner::Retire`, see [`older`]); otherwise the command runs its
/// browser itself. `Release` and `Retire` reach an owner of any build.
///
/// The version and the executable's size and modification time, which a
/// reinstall changes. Fixed the first time it is read, which `owner::install`
/// and `owner::serve` do as the process starts: an owner reports the binary
/// it was started from after that file is replaced under it, and a command
/// running old code never claims the new binary's identity.
pub(super) fn build() -> String {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD
        .get_or_init(|| {
            let file = std::env::current_exe()
                .and_then(std::fs::metadata)
                .ok()
                .map(|meta| {
                    let modified = meta
                        .modified()
                        .ok()
                        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |since| since.as_nanos());
                    format!("{} {modified}", meta.len())
                })
                .unwrap_or_default();
            format!("{} {file}", env!("CARGO_PKG_VERSION"))
        })
        .clone()
}

/// Whether an owner of build `theirs` runs a binary older than `ours`, by
/// the modification time [`build`] ends with: the one a command may retire.
/// Only ever the older one, so two builds used side by side — a monitor
/// started before an upgrade and a command after it — never retire each
/// other's owner in turn; the newer keeps it. A build that says no time is
/// the oldest there is.
pub(super) fn older(theirs: &str, ours: &str) -> bool {
    let modified = |build: &str| {
        build
            .rsplit(' ')
            .next()
            .and_then(|last| last.parse::<u128>().ok())
            .unwrap_or(0)
    };
    modified(theirs) < modified(ours)
}

/// From a command to the owner.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ToOwner {
    /// The first message on every connection.
    Hello { build: String },
    /// A request, to send as `session` from the account's browser.
    Send {
        id: u64,
        session: Box<Session>,
        request: PageRequest,
    },
    /// An intent, to answer as `session` from the account's browser: the
    /// request it names built in the tab and sent, or read from the tab's
    /// document.
    Ask {
        id: u64,
        session: Box<Session>,
        call: Box<Call>,
    },
    /// What the account's browser holds now, for the command to write back:
    /// only while it holds the session `handed` names, the one the command
    /// sent as.
    Cookies { id: u64, pk: Pk, handed: String },
    /// The command is done with the browsers it used.
    Leave { id: u64 },
    /// Close the account's browser, or every one, before something removes or
    /// replaces the profiles they run on.
    Release { id: u64, pk: Option<Pk> },
    /// Stop taking commands, and leave once the ones connected are done: a
    /// command of a newer build found this owner with nobody else to serve,
    /// or a purge is about to take its files.
    Retire,
}

/// From the owner to a command.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum FromOwner {
    /// The answer to `Hello`, with how many other commands are connected:
    /// one of a newer build asks the owner to leave only when there are none.
    Welcome {
        build: String,
        #[serde(default)]
        others: usize,
    },
    /// The answer to `Send`.
    Answer {
        id: u64,
        answer: Result<PageResponse, PageError>,
    },
    /// The answer to `Ask`.
    Told {
        id: u64,
        told: Result<Box<Told>, PageError>,
    },
    /// The answer to `Cookies`: nothing for an account with no browser open,
    /// or one holding another session than the command handed it.
    Cookies {
        id: u64,
        cookies: Option<BrowserCookies>,
    },
    /// `Leave` or `Release` has been carried out.
    Done { id: u64 },
    /// Instagram pushed back on an account's browser. Sent to every command
    /// the moment it is heard, not in answer to anything.
    Heard { pk: Pk, cause: PushedBack },
}

impl FromOwner {
    /// The request this answers, if it answers one.
    pub(super) fn id(&self) -> Option<u64> {
        match self {
            Self::Answer { id, .. }
            | Self::Told { id, .. }
            | Self::Cookies { id, .. }
            | Self::Done { id } => Some(*id),
            Self::Welcome { .. } | Self::Heard { .. } => None,
        }
    }
}

/// Counts what is written to it, to size a frame before it is built.
struct Measure(usize);

impl std::io::Write for Measure {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One message, framed and ready to write in a single piece: a writer that
/// only ever writes whole frames cannot leave half of one on the stream.
///
/// Measured first and allocated once: a buffer that grew as the message was
/// written would leave its earlier allocations, the session in them, freed
/// unwiped.
pub(super) fn frame(message: &impl Serialize) -> std::io::Result<Zeroizing<Vec<u8>>> {
    let mut measure = Measure(0);
    serde_json::to_writer(&mut measure, message)?;
    let length = measure.0;
    if length > MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a message too large to send",
        ));
    }
    let mut frame = Zeroizing::new(Vec::with_capacity(4 + length));
    frame.extend_from_slice(&(length as u32).to_be_bytes());
    serde_json::to_writer(&mut *frame, message)?;
    debug_assert_eq!(frame.len(), 4 + length, "the message wrote the same twice");
    Ok(frame)
}

/// Writes one framed message.
pub(super) async fn write(to: &mut (impl AsyncWrite + Unpin), frame: &[u8]) -> std::io::Result<()> {
    to.write_all(frame).await?;
    to.flush().await
}

/// Reads one message; `None` when the other side closed the connection
/// between two of them.
pub(super) async fn read<T: serde::de::DeserializeOwned>(
    from: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<T>> {
    let Some(body) = read_body(from).await? else {
        return Ok(None);
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Reads one message's JSON, unparsed; `None` as [`read`].
pub(super) async fn read_body(
    from: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<Zeroizing<Vec<u8>>>> {
    let mut length = [0u8; 4];
    match from.read_exact(&mut length).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a message of {length} bytes, past the ceiling"),
        ));
    }
    let mut body = Zeroizing::new(vec![0u8; length]);
    from.read_exact(&mut body).await?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::session::SessionOrigin;
    use snob_ig::client::page::Method;

    fn session() -> Session {
        Session::from_sessionid(
            "42%3Aabc%3A1",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/141.0.0.0 Safari/537.36",
            SessionOrigin::Paste,
        )
        .expect("a valid session")
    }

    fn request() -> PageRequest {
        PageRequest {
            method: Method::Post,
            url: "https://www.instagram.com/api/v1/x/".to_string(),
            headers: vec![("x-ig-app-id".to_string(), "936619743392459".to_string())],
            referrer: "https://www.instagram.com/".to_string(),
            body: Some("a=1".to_string()),
            navigate: false,
            cap: 1024,
            timeout_ms: 30_000,
        }
    }

    #[tokio::test]
    async fn a_request_arrives_as_it_was_sent() {
        let sent = ToOwner::Send {
            id: 7,
            session: Box::new(session()),
            request: request(),
        };
        let bytes = frame(&sent).unwrap();
        assert_eq!(
            bytes.len(),
            bytes.capacity(),
            "one allocation, the size of the frame"
        );
        let mut reader = &bytes[..];
        let Some(ToOwner::Send {
            id,
            session: got,
            request: asked,
        }) = read::<ToOwner>(&mut reader).await.unwrap()
        else {
            panic!("not a request");
        };
        assert_eq!(id, 7);
        assert_eq!(got.sessionid.expose(), "42%3Aabc%3A1");
        assert_eq!(got.fingerprint(), session().fingerprint());
        assert_eq!(asked.method, Method::Post);
        assert_eq!(asked.body.as_deref(), Some("a=1"));
        assert_eq!(asked.headers, request().headers);
        // Nothing left over: one frame is one message.
        assert!(read::<ToOwner>(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn every_answer_comes_back_as_itself() {
        let answers: Vec<Result<PageResponse, PageError>> = vec![
            Ok(PageResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: "{\"ok\":\"\u{e9}\u{1f600}\\u0000\"}".to_string(),
                url: "https://www.instagram.com/x".to_string(),
                redirected: true,
                hops: 2,
                too_large: false,
            }),
            Err(PageError::Unreachable("dns".to_string())),
            Err(PageError::NoCsrfToken),
            Err(PageError::Browser("gone".to_string())),
            Err(PageError::PushedBack(PushedBack::Challenge {
                url: Some("https://www.instagram.com/challenge/".to_string()),
            })),
        ];
        for answer in answers {
            let wanted = format!("{answer:?}");
            let bytes = frame(&FromOwner::Answer { id: 1, answer }).unwrap();
            let Some(FromOwner::Answer { answer, .. }) =
                read::<FromOwner>(&mut &bytes[..]).await.unwrap()
            else {
                panic!("not an answer");
            };
            assert_eq!(format!("{answer:?}"), wanted);
        }
    }

    /// An intent arrives as it was sent, and each answer to one comes back as
    /// itself.
    #[tokio::test]
    async fn an_intent_and_its_answer_arrive_as_they_were_sent() {
        use snob_ig::allowlist::Operation;
        use snob_ig::model::web::RouteAnswer;
        use snob_ig::web::Ask;

        let call = Call {
            ask: Ask::Query {
                operation: Operation::HoverCard,
                variables: r#"{"userID":"9001"}"#.to_string(),
            },
            origin: "https://www.instagram.com".to_string(),
            referrer: "/someone/".to_string(),
            claim: "0".to_string(),
            cap: 1024,
            timeout_ms: 30_000,
        };
        let sent = ToOwner::Ask {
            id: 9,
            session: Box::new(session()),
            call: Box::new(call.clone()),
        };
        let bytes = frame(&sent).unwrap();
        let Some(ToOwner::Ask {
            id,
            session: got,
            call: asked,
        }) = read::<ToOwner>(&mut &bytes[..]).await.unwrap()
        else {
            panic!("not an intent");
        };
        assert_eq!(id, 9);
        assert_eq!(got.fingerprint(), session().fingerprint());
        assert_eq!(*asked, call);

        let answer = PageResponse {
            status: 200,
            body: "{}".to_string(),
            ..PageResponse::default()
        };
        let told: Vec<Result<Box<Told>, PageError>> = vec![
            Ok(Box::new(Told::Answer(answer.clone()))),
            Ok(Box::new(Told::Viewer(snob_ig::page_values::Viewer {
                pk: Pk::new(42),
                username: "me".to_string(),
                fbid: "17841400000000042".to_string(),
            }))),
            Ok(Box::new(Told::Tray(Some(vec!["9001".to_string()])))),
            Ok(Box::new(Told::Pk {
                answer: answer.clone(),
                pk: RouteAnswer::NoProfile,
            })),
            Ok(Box::new(Told::Document {
                answer,
                bundles: Vec::new(),
            })),
            Err(PageError::NotReady("the web session id".to_string())),
            Err(PageError::LoggedOut),
            Err(PageError::NotAllowed("POST /api/graphql".to_string())),
        ];
        for told in told {
            let wanted = format!("{told:?}");
            let bytes = frame(&FromOwner::Told { id: 3, told }).unwrap();
            let Some(FromOwner::Told { id, told }) =
                read::<FromOwner>(&mut &bytes[..]).await.unwrap()
            else {
                panic!("not an answer to an intent");
            };
            assert_eq!(id, 3);
            assert_eq!(format!("{told:?}"), wanted);
        }
    }

    #[tokio::test]
    async fn a_length_past_the_ceiling_is_refused_unread() {
        let mut claim = ((MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes().to_vec();
        claim.extend_from_slice(b"{}");
        let refused = read::<FromOwner>(&mut &claim[..]).await.unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_message_cut_short_is_an_error_and_a_clean_close_is_not() {
        let bytes = frame(&ToOwner::Retire).unwrap();
        let cut = &bytes[..bytes.len() - 1];
        assert!(read::<ToOwner>(&mut &cut[..]).await.is_err());
        assert!(read::<ToOwner>(&mut &b""[..]).await.unwrap().is_none());
    }

    fn framed(json: &str) -> Vec<u8> {
        let mut bytes = (json.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(json.as_bytes());
        bytes
    }

    #[tokio::test]
    async fn only_the_two_methods_cross() {
        let mut sent = serde_json::to_value(ToOwner::Send {
            id: 1,
            session: Box::new(session()),
            request: request(),
        })
        .unwrap();
        sent["request"]["method"] = "DELETE".into();
        let bytes = framed(&sent.to_string());
        assert!(read::<ToOwner>(&mut &bytes[..]).await.is_err());
    }

    /// What another build says still reads: a field added later is ignored by
    /// an older reader, and one missing from an older writer takes its
    /// default.
    #[tokio::test]
    async fn the_handshake_reads_across_builds() {
        let hello = framed(r#"{"kind":"hello","build":"0.6.0 1 2","later":true}"#);
        let Some(ToOwner::Hello { build }) = read::<ToOwner>(&mut &hello[..]).await.unwrap() else {
            panic!("not a hello");
        };
        assert_eq!(build, "0.6.0 1 2");
        let welcome = framed(r#"{"kind":"welcome","build":"0.5.0 1 2"}"#);
        let Some(FromOwner::Welcome { others, .. }) =
            read::<FromOwner>(&mut &welcome[..]).await.unwrap()
        else {
            panic!("not a welcome");
        };
        assert_eq!(others, 0);
        let retire = framed(r#"{"kind":"retire"}"#);
        assert!(matches!(
            read::<ToOwner>(&mut &retire[..]).await.unwrap(),
            Some(ToOwner::Retire)
        ));
        let release = framed(r#"{"kind":"release","id":3,"pk":null}"#);
        assert!(matches!(
            read::<ToOwner>(&mut &release[..]).await.unwrap(),
            Some(ToOwner::Release { id: 3, pk: None })
        ));
        let done = framed(r#"{"kind":"done","id":3}"#);
        assert!(matches!(
            read::<FromOwner>(&mut &done[..]).await.unwrap(),
            Some(FromOwner::Done { id: 3 })
        ));
    }

    /// A message of a kind this build does not know is read whole, so the one
    /// after it reads as it was sent: a command skips it and goes on.
    #[tokio::test]
    async fn a_message_this_build_cannot_read_is_read_past() {
        let mut bytes = framed(r#"{"kind":"heard_more","pk":42}"#);
        bytes.extend_from_slice(&frame(&FromOwner::Done { id: 3 }).unwrap());
        let mut from = &bytes[..];
        let unknown = read_body(&mut from).await.unwrap().unwrap();
        assert!(serde_json::from_slice::<FromOwner>(&unknown).is_err());
        assert!(matches!(
            read::<FromOwner>(&mut from).await.unwrap(),
            Some(FromOwner::Done { id: 3 })
        ));
    }

    /// Only the older binary's owner is retired, whichever of the two asks.
    #[test]
    fn only_an_older_build_is_retired() {
        assert!(older("0.6.0 100 1000", "0.6.0 100 2000"));
        assert!(
            !older("0.6.0 100 2000", "0.6.0 100 1000"),
            "a monitor of the old build leaves the new one's owner alone"
        );
        assert!(!older("0.6.0 100 1000", "0.6.0 200 1000"), "nor a twin");
        assert!(older("0.6.0 ", "0.6.0 100 1000"), "no time is the oldest");
        assert!(!older("0.6.0 100 1000", "0.6.0 "));
    }
}
