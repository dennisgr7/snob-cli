//! A tab with no browser behind it, on the fake Instagram of [`super::ig`].
//!
//! It does what the headless tab does, the browser aside: a request is sent
//! over `reqwest` with the session's cookies, `X-CSRFToken` written in from
//! the cookie as the page's fetch writes it; an intent is built with the
//! same [`snob_ig::web::build_call`] the tab runs, from the values of the
//! document it is on and the app's calls on it. A document is not loaded
//! but rendered by the world, and the app's boot calls are taken in as the
//! listener takes them, without being sent. A document hands back no bundle
//! URLs, so nothing is ever downloaded for one. A write is built only on
//! the document the last `Ask::Document` loaded at its referrer.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine as _;
use std::sync::atomic::{AtomicU64, Ordering};

use snob_core::Pk;
use snob_core::session::Session;
use snob_ig::allowlist::Operation;
use snob_ig::client::page::{
    AskFuture, Method, Page, PageError, PageFuture, PageRequest, PageResponse,
};
use snob_ig::model::web::{RouteAnswer, document_pk, route_answer, tray_ids};
use snob_ig::page_values::PageValues;
use snob_ig::web::{AppCalls, Ask, Call, Seen, Told, build_call, made_from};

use super::ig::{CSRF, World};

/// The document the tab is on.
struct Document {
    /// Which load of the tab it is, as a loader names a document.
    load: u64,
    /// The path the tab is on.
    path: String,
    html: String,
    status: u16,
    values: PageValues,
    calls: AppCalls,
}

/// A tab on the world's documents, logged in as one session.
pub struct TestTab {
    world: World,
    http: reqwest::Client,
    sessionid: String,
    pk: Pk,
    document: Mutex<Option<Document>>,
    /// The latest variables the app sent with each registry query, from any
    /// document.
    templates: Mutex<HashMap<Operation, String>>,
    /// How many documents the tab has loaded.
    loads: AtomicU64,
    /// The document the last `Ask::Document` loaded, by its load, and the
    /// path it was asked for: the only one a write may be built on.
    asked_document: Mutex<Option<(u64, String)>>,
}

impl TestTab {
    pub fn new(world: &World, session: &Session) -> Self {
        Self {
            world: world.clone(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client for the fake"),
            sessionid: session.sessionid.expose().to_string(),
            pk: session.ds_user_id,
            document: Mutex::new(None),
            templates: Mutex::new(HashMap::new()),
            loads: AtomicU64::new(0),
            asked_document: Mutex::new(None),
        }
    }

    /// The tab goes to `path`: the world renders it for this session, and
    /// the app's boot calls on it are taken in, unless the world has the
    /// app make none.
    fn load(&self, path: &str) -> Document {
        let viewer = self.world.viewer_of(&self.sessionid);
        let rendered = self.world.render(viewer, path, "");
        let mut values = PageValues::parse(&rendered.html);
        let mut calls = AppCalls::default();
        if !self.world.is_app_calls_missing() {
            let mut templates = self.templates.lock().unwrap();
            for (to, form) in &rendered.forms {
                if let Some(operation) = calls.saw(to, form) {
                    let sent = calls.variables_sent(operation).unwrap_or_default();
                    templates.insert(operation, sent.to_string());
                }
                if values.web_session.is_none() {
                    values.web_session = url::form_urlencoded::parse(form.as_bytes())
                        .find(|(n, _)| n == "__s")
                        .map(|(_, v)| v.into_owned());
                }
            }
        }
        Document {
            load: self.loads.fetch_add(1, Ordering::SeqCst) + 1,
            path: path.to_string(),
            html: rendered.html,
            status: rendered.status,
            values,
            calls,
        }
    }

    /// Runs `with` on the document the tab is on, the home page until it
    /// has gone anywhere.
    fn on_document<T>(&self, with: impl FnOnce(&mut Document) -> T) -> T {
        let mut document = self.document.lock().unwrap();
        let document = document.get_or_insert_with(|| self.load("/"));
        with(document)
    }

    fn navigated(&self, path: &str) -> PageResponse {
        let document = self.load(path);
        let answer = PageResponse {
            status: document.status,
            url: path.into(),
            ..Default::default()
        };
        *self.document.lock().unwrap() = Some(document);
        answer
    }

    fn signed_in(&self) -> Result<snob_ig::page_values::Viewer, PageError> {
        let values = self.on_document(|d| d.values.clone());
        Ok(snob_ig::web::viewer_of(&values, self.pk)?.clone())
    }

    /// The request `call` asks for, built on the tab's document.
    ///
    /// As the headless tab builds it: a read about an account the tab is not
    /// showing is made from the page the tab is on, and copies the variables
    /// the app last sent with the same operation.
    pub fn build(&self, call: &Call) -> Result<PageRequest, PageError> {
        let variables = match &call.ask {
            Ask::Query { operation, .. } => self.templates.lock().unwrap().get(operation).cloned(),
            _ => None,
        };
        self.on_document(|d| {
            let call = made_from(call, &d.path);
            let rest_token = d.calls.rest_token_sent().map(str::to_string);
            let seen = Seen {
                variables: variables.as_deref(),
                doc_id: None,
                rest_token: rest_token.as_deref(),
            };
            build_call(&call, &d.values, &mut d.calls, self.pk, &seen)
        })
        .map_err(PageError::from)
    }

    async fn answer(&self, call: Call) -> Result<Told, PageError> {
        match &call.ask {
            Ask::Viewer => self.signed_in().map(Told::Viewer),
            Ask::Tray => {
                self.signed_in()?;
                Ok(Told::Tray(self.on_document(|d| tray_ids(&d.html))))
            }
            Ask::Document { path } => {
                *self.asked_document.lock().unwrap() = None;
                let mut answer = self.navigated(path);
                answer.url = format!("{}{path}", call.origin);
                if (200..300).contains(&answer.status) {
                    self.signed_in()?;
                    let load = self.on_document(|d| d.load);
                    *self.asked_document.lock().unwrap() = Some((load, path.clone()));
                }
                Ok(Told::Document {
                    answer,
                    bundles: Vec::new(),
                })
            }
            Ask::Pk { name } => {
                self.signed_in()?;
                let route = format!("/{name}/");
                if self.on_document(|d| d.calls.made_a_route_call()) {
                    let request = self.build(&call)?;
                    let answer = self.send_now(request).await?;
                    let pk = route_answer(&answer.body, &route);
                    return Ok(Told::Pk { answer, pk });
                }
                let mut answer = self.navigated(&route);
                answer.url = format!("{}{route}", call.origin);
                let (from_the_document, from_the_app) = self.on_document(|d| {
                    let app = d
                        .calls
                        .variables_sent(Operation::ProfilePage)
                        .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                        .and_then(|v| v.get("id")?.as_str()?.parse::<Pk>().ok());
                    (document_pk(&d.html), app)
                });
                let pk = match (from_the_document, from_the_app) {
                    (Some(document), Some(app)) if document != app => RouteAnswer::Error,
                    (Some(pk), _) | (None, Some(pk)) => RouteAnswer::Pk(pk),
                    (None, None) => RouteAnswer::Error,
                };
                Ok(Told::Pk { answer, pk })
            }
            Ask::Write { .. } => {
                let load = self.on_document(|d| d.load);
                let made_from = self.asked_document.lock().unwrap().clone();
                if made_from != Some((load, call.referrer.clone())) {
                    return Err(PageError::NotReady(
                        "the profile this write is made from".into(),
                    ));
                }
                let request = self.build(&call)?;
                self.send_now(request).await.map(Told::Answer)
            }
            Ask::Query { .. }
            | Ask::Rest { .. }
            | Ask::Navigation { .. }
            | Ask::Statuses { .. } => {
                let request = self.build(&call)?;
                self.send_now(request).await.map(Told::Answer)
            }
            // As the headless tab fetches one: held to the rule the tab
            // holds, with none of the session's cookies, handed back as
            // base64 and `too_large` without a body past the cap.
            Ask::Asset { url } => {
                if let Some(what) = snob_ig::allowlist::refused_asset(&call.origin, url) {
                    return Err(PageError::NotAllowed(what));
                }
                let unreachable = |e: reqwest::Error| PageError::Unreachable(e.to_string());
                let response = self.http.get(url).send().await.map_err(unreachable)?;
                let status = response.status().as_u16();
                let bytes = response.bytes().await.map_err(unreachable)?;
                let too_large = bytes.len() as u64 > call.cap;
                Ok(Told::Asset(PageResponse {
                    status,
                    body: if too_large {
                        String::new()
                    } else {
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    },
                    url: url.clone(),
                    too_large,
                    ..Default::default()
                }))
            }
        }
    }

    /// Sends `request` as the page would: the session's cookies, the CSRF
    /// token written in from the cookie where the request names one, no
    /// redirect followed on a fetch, and a navigation's followed and
    /// counted.
    async fn send_now(&self, request: PageRequest) -> Result<PageResponse, PageError> {
        let mut url = request.url.clone();
        let mut hops = 0;
        loop {
            let method = match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
            };
            let mut sending = self
                .http
                .request(method, &url)
                .header(
                    "Cookie",
                    format!(
                        "sessionid={}; ds_user_id={}; csrftoken={CSRF}",
                        self.sessionid, self.pk
                    ),
                )
                .header("Referer", &request.referrer);
            for (name, value) in &request.headers {
                let value = if name.eq_ignore_ascii_case("x-csrftoken") {
                    CSRF
                } else {
                    value.as_str()
                };
                sending = sending.header(name, value);
            }
            if request.navigate {
                sending = sending.header("Sec-Fetch-Mode", "navigate");
            }
            if let Some(body) = &request.body {
                sending = sending.body(body.clone());
            }
            let response = sending
                .send()
                .await
                .map_err(|e| PageError::Unreachable(e.to_string()))?;
            let status = response.status().as_u16();
            if response.status().is_redirection() {
                if !request.navigate {
                    return Ok(PageResponse {
                        status: 0,
                        ..Default::default()
                    });
                }
                let to = response
                    .headers()
                    .get("location")
                    .and_then(|l| l.to_str().ok())
                    .unwrap_or_default();
                url = url::Url::parse(&url)
                    .and_then(|u| u.join(to))
                    .map_err(|e| PageError::Unreachable(e.to_string()))?
                    .to_string();
                hops += 1;
                if hops > 10 {
                    return Err(PageError::Unreachable("too many redirects".into()));
                }
                continue;
            }
            let headers = response
                .headers()
                .iter()
                .filter(|(n, _)| n.as_str() != "set-cookie")
                .filter_map(|(n, v)| Some((n.as_str().to_string(), v.to_str().ok()?.to_string())))
                .collect();
            let body = response
                .text()
                .await
                .map_err(|e| PageError::Unreachable(e.to_string()))?;
            let too_large = body.len() as u64 > request.cap;
            if request.navigate {
                let path = url::Url::parse(&url)
                    .map(|u| u.path().to_string())
                    .unwrap_or_default();
                *self.document.lock().unwrap() = Some(self.load(&path));
            }
            return Ok(PageResponse {
                status,
                headers,
                body: if too_large { String::new() } else { body },
                redirected: hops > 0,
                hops,
                url,
                too_large,
            });
        }
    }
}

impl Page for TestTab {
    fn send(&self, request: PageRequest) -> PageFuture<'_> {
        Box::pin(self.send_now(request))
    }

    fn ask(&self, call: Call) -> AskFuture<'_> {
        Box::pin(self.answer(call))
    }
}
