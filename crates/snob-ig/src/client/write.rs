//! The follow and the unfollow, and everything only a write needs.
//!
//! **One file, so that the rule can be checked by opening it.** `AGENTS.md`
//! says a write leaves only through [`IgClient::post`], without a browser, or
//! as a [`web::Ask::Write`] asked of the page; this is the only file that
//! does either, and the two mutations below are the only things that reach
//! them.
//!
//! What can be sent is a [`graphql::Mutation`] rather than a path, so the set
//! of writes this program can make is the set of variants that enum has --
//! which is what makes a third one a compile error rather than a string
//! somebody typed.

use serde::de::DeserializeOwned;
use snob_core::Pk;

use crate::error::IgError;
use crate::graphql;
use crate::model::{FriendshipResult, FriendshipStatus};
use crate::page_values::PageValues;
use crate::web;

use super::IgClient;
use super::ask::{answered, told_otherwise};
use super::headers::Surface;
use super::transport::{Answer, MAX_BODY_BYTES, load_of, read_capped, retry_after, served_of};

/// Whether a refused mutation is worth spending a discovery walk on.
///
/// **Narrow on purpose.** A rotated identifier and a throttled account both
/// come back as a refusal, and walking megabytes of JavaScript at an account
/// Instagram has just said no to is the opposite of what the pacing rules are
/// for. So everything that means "stop asking" is excluded: a push-back, an
/// action block, a challenge, a dead session, a cancellation. What is left is
/// the shapes a bad `doc_id` actually takes — a 400 with a body that did not
/// parse, or one that did and said nothing useful.
/// **`NotFound` is in here and is not in `IgError::worth_a_second_route`**, and
/// the two are not contradicting each other. There, a 404 means the
/// account does not exist and a second lookup would be a second request spent
/// confirming it. Here, a 404 is one of the ways Instagram refuses an operation
/// it no longer serves under that identifier, and the discovery walk costs it
/// nothing: it reads the CDN, not the API, so a wrong guess is bytes rather
/// than a request against the account's budget.
///
/// **And narrow in a second direction, which is the one that matters for a
/// write.** The walk ends in a second attempt at the mutation, and a second
/// mutation is only safe when the first is known not to have happened. A 4xx
/// and a 404 say so; a 200 carrying `errors` says so -- that is a GraphQL
/// refusal, and the shape a stale identifier actually arrives in -- unless
/// the errors say not to repeat it ([`IgError::QueryRefused`]). A 5xx does
/// not: an edge that answers 502 may have forwarded the request before it
/// failed. Nor does the redirect either arm reports (an `Unexpected` 3xx
/// from `post`, status 0 from the page), nor a 200 whose body would not
/// decode, which is a write that very probably happened and an answer this
/// program could not read. Letting any of those through would send the
/// follow again on a budget paid once and a confirmation given once -- the
/// replay the redirect policy exists to prevent, arriving by another road.
fn worth_rediscovering(error: &IgError) -> bool {
    match error {
        IgError::NotFound { .. } => true,
        IgError::Unexpected { status, .. } => {
            (200..300).contains(status) || (400..500).contains(status)
        }
        _ => false,
    }
}

impl IgClient {
    /// **The only function in this workspace that sends anything other than a
    /// GET to Instagram without a browser.** Everything the write rule in
    /// `AGENTS.md` promises is enforced on the way through here; from the
    /// page, [`IgClient::ask_page`] keeps the same promises for a
    /// [`web::Ask::Write`], and this refuses to send.
    ///
    /// What it does that [`IgClient::get`] does not, each of them there
    /// because a write is not a read:
    ///
    /// - **It pays the write budget**, not the read one, and like `get` it pays
    ///   before it sends. There is no argument that selects between the two:
    ///   `get` calls `clear_to_send` and this calls `clear_to_send_write`, so
    ///   the choice is made by which function the caller reached rather than by
    ///   a value it passed.
    /// - **It refuses without a CSRF token instead of finding out.** Instagram
    ///   would answer 403, and that 403 would cost a request, a slot of write
    ///   budget and — because `classify` reads a 403 as a dead session — a
    ///   message telling the user to log in again when their session is fine.
    ///   Sent with `reqwest` (`SNOB_NO_BROWSER`, or a test server), a `snob
    ///   login --paste` session has none unless `--csrftoken` gave it one.
    /// - **It sends `Origin`**, which the Fetch standard requires on a POST even
    ///   when the request is same-origin. See the comment in
    ///   [`IgClient::dressed`] for the half of that rule which lives on
    ///   the read side.
    /// - **It follows no redirect at all**: the API client's policy is
    ///   `none()`, and [`super::transport::api_policy`] says why a write
    ///   depends on that.
    /// - **It is not abandoned once it is in flight**, and that is one of the
    ///   two places this program does not do what `AGENTS.md` says about
    ///   Ctrl+C; the other is [`IgClient::ask_page`], for a
    ///   [`web::Ask::Write`] from the page.
    ///   Every read races the cancel token against the socket, because giving
    ///   up on a read costs nothing: the answer was going to be thrown away.
    ///   Giving up on a write costs the one thing worth having, which is
    ///   knowing whether it happened — the request has already gone, Instagram
    ///   may well act on it, and reporting "canceled" to somebody who is now
    ///   following an account is worse than making them wait out the timeout.
    ///
    ///   The boundary is exact rather than convenient: `clear_to_send_write`
    ///   reads the token before it reserves and again inside the wait, so a
    ///   write is cancelable up to the moment it is sent and not after it. The
    ///   uncancelable part is the part that must not be abandoned.
    /// - **It cannot be pointed anywhere.** It takes a
    ///   [`graphql::Mutation`], not a path; the crate doc says why that is what
    ///   makes a third write a compile error.
    ///
    /// `referer` is the profile page the button would have been clicked on, in
    /// the same spelling `get` wants: a path with no leading slash.
    async fn post<T: DeserializeOwned>(
        &self,
        write: graphql::Mutation,
        form: &[(&str, &str)],
        referer: &str,
        style: Surface<'_>,
    ) -> Result<T, IgError> {
        // Before the budget is charged, so that a session which cannot write
        // does not spend a slot discovering it. The token itself is put on the
        // request by `dressed`, which adds it whenever the session has
        // one; this guard is what makes "whenever" mean "always" on this path.
        if !self.can_write() {
            return Err(IgError::NoCsrfToken);
        }
        // From the page, a write is asked of the tab, which builds it beside
        // the document's values ([`IgClient::write_from_the_page`]); it is
        // never sent as a request built here.
        if self.page.is_some() {
            return Err(IgError::NotAllowed(format!(
                "POST {} built outside the tab",
                write.path()
            )));
        }

        let url = self.base.join(write.path())?;
        tracing::debug!(%url, operation = write.friendly_name(), "POST");
        let endpoint = url.path().to_string();

        self.pacer.clear_to_send_write().await?;

        let request = self
            .dressed(self.api.post(url), referer, style)
            .header("Origin", self.base.as_str().trim_end_matches('/'))
            .form(form);

        let response = request.send().await?;
        self.remember_claim(&response);

        let status = response.status();
        // Read before the body, for the same reason `get_body` reads it there:
        // the header is on the response, and a body that will not read must not
        // take the one measurement worth having away with it.
        let retry_after = retry_after(&response);
        let load = load_of(&response);
        let served = served_of(&response);

        // A redirect reaches here as a status rather than as a new request,
        // because the policy is `none()`. It is not success and it is not
        // something to replay, so it is reported as what it is.
        if status.is_redirection() {
            return Err(IgError::Unexpected {
                status: status.as_u16(),
                body: "Instagram redirected a write, which is never followed".into(),
            });
        }

        let body = match read_capped(response, MAX_BODY_BYTES).await {
            Ok(body) => body,
            Err(_) if !status.is_success() => String::new(),
            Err(e) => return Err(e),
        };

        self.decode(&Answer {
            endpoint,
            status: status.as_u16(),
            body,
            retry_after,
            load,
            served,
        })
    }

    /// Follows an account. **A write.** See [`IgClient::post`], and
    /// [`IgClient::write_from_the_page`] for a browser.
    pub async fn follow(
        &self,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        self.friendship(graphql::Mutation::Follow, pk, username, ids)
            .await
    }

    /// Unfollows an account. **A write.** See [`IgClient::post`], and
    /// [`IgClient::write_from_the_page`] for a browser.
    pub async fn unfollow(
        &self,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        self.friendship(graphql::Mutation::Unfollow, pk, username, ids)
            .await
    }

    /// The `doc_id` of a mutation, out of the page's own JavaScript.
    ///
    /// The walk goes through the **CDN client**, which is the right one twice
    /// over: those bundles really are on the CDN, and that client carries no
    /// cookie and no app id -- a public script has no business being fetched
    /// with a session attached. It also means they are not charged against
    /// Instagram's request budget, for the reason [`IgClient::download`]
    /// already gives about pictures.
    ///
    /// `bundles` are the ones the page the write is made from names, in the
    /// page's order, which puts the chunk its route needs near the front: read
    /// off the HTML without a browser, and handed back by the tab with one.
    /// **Only those [`graphql::is_bundle`] takes are opened**, whatever list
    /// they came in: the host rule is the command's, so a list that crossed
    /// the owner's socket cannot point the walk anywhere else.
    async fn doc_id_for(
        &self,
        mutation: graphql::Mutation,
        bundles: &[String],
        ids: &dyn graphql::DocIds,
    ) -> Result<String, IgError> {
        /// One bundle. Instagram's largest is around five megabytes; twelve is
        /// a ceiling rather than a target, and it is here for the reason every
        /// other ceiling in this file is.
        const MAX_BUNDLE_BYTES: usize = 12 * 1024 * 1024;
        /// How many to open before giving up.
        ///
        /// **A page names around four hundred and forty of these**, so this is
        /// a real bound rather than a formality: the walk is bytes off the CDN
        /// and minutes of wall clock, and it happens once per rotation because
        /// the answer is cached. Sixty is roughly seven megabytes, measured.
        /// Past that the answer is more likely to be that the shape changed
        /// than that the right chunk is one further along.
        const MOST_BUNDLES: usize = 60;

        let name = mutation.friendly_name();

        let walked = bundles.iter().filter(|url| graphql::is_bundle(url));
        for url in walked.take(MOST_BUNDLES) {
            let bytes = match self.download_capped(url, MAX_BUNDLE_BYTES).await {
                Ok(bytes) => bytes,
                // The user stopping it is not a bundle that would not come
                // down. Read as one, an interrupt here would churn through the
                // remaining sixty and then report Instagram's original
                // refusal, with its exit code, for what was a Ctrl+C.
                Err(IgError::Canceled) => return Err(IgError::Canceled),
                // A bundle that will not come down is not the end of the
                // search: there are others, and the next may hold it.
                Err(_) => continue,
            };
            if let Some(id) = graphql::doc_id_in(&String::from_utf8_lossy(&bytes), name) {
                tracing::debug!(name, %url, "found the mutation id");
                ids.put(name, &id);
                return Ok(id);
            }
        }

        Err(IgError::MutationNotFound { name })
    }

    /// The body of both, because the two differ by one word.
    ///
    /// # Which request
    ///
    /// `POST /api/graphql`, naming the mutation, as the real web client sent it
    /// in an August 2026 capture -- see [`crate::graphql`], which carries the
    /// finding and the two REST routes that change nothing.
    ///
    /// Two requests, and both are paid for: the page the write is made from,
    /// and the mutation. The page is the expensive one at around six hundred
    /// kilobytes, and it is why there is no bulk mode to be tempted by even if
    /// the rule allowed one.
    ///
    /// The profile loaded is **the target's**, which is the page a browser
    /// would have been on when the button was pressed. From the browser that
    /// is what the write is made from: its route, its referrer and the app's
    /// calls on it go into the form. Without one, any logged-in page would
    /// hand out the same tokens, so it is coherence rather than necessity --
    /// but a request whose `Referer` names a page nobody loaded is the kind of
    /// small incoherence the rest of this module exists to avoid.
    ///
    /// The `doc_id` is what is known, and otherwise what was captured.
    /// [`graphql::Mutation::seed_doc_id`] explains why there is a captured
    /// value at all, and why discovery is the recovery rather than the way in.
    ///
    /// **Nothing is read after the write**: the answer carries the
    /// relationship it left, and that is the result.
    async fn friendship(
        &self,
        mutation: graphql::Mutation,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        // **Before the page.** `post` refuses without a CSRF token too, but by
        // then the expensive half has been spent: a profile page is around six
        // hundred kilobytes and a paid-for request. Without the browser, every
        // `snob login --paste` without `--csrftoken` is such a session.
        if !self.can_write() {
            return Err(IgError::NoCsrfToken);
        }

        let referer = if username.is_empty() {
            String::new()
        } else {
            format!("{}/", snob_core::model::in_a_path(username))
        };

        if self.page.is_some() {
            return self
                .write_from_the_page(mutation, pk, &format!("/{referer}"), ids)
                .await;
        }

        let html = self.page(&format!("/{referer}")).await?;
        // **Whose page it is, before any token is read off it.** A logged-out
        // page carries both tokens, so the tokens alone do not tell one apart
        // (`graphql::extract_tokens`); its `PolarisViewer` does. A page served
        // to nobody, to an unread viewer, or to another account cannot
        // authorize a write as this session, so it is reported as a dead
        // session and nothing is posted.
        let values = PageValues::parse(&html);
        if web::viewer_of(&values, self.session.ds_user_id).is_err() {
            return Err(IgError::SessionExpired);
        }
        // A page without the tokens cannot authorize a mutation either.
        let tokens = graphql::extract_tokens(&html).ok_or(IgError::SessionExpired)?;

        let doc_id = Self::known_doc_id(mutation, ids);
        let bundles = graphql::bundles_in(&html);
        self.rediscovering(mutation, doc_id, &bundles, ids, async |doc_id: &str| {
            self.mutate(mutation, &tokens, doc_id, pk, &referer).await
        })
        .await
    }

    /// A write from the browser: the tab loads the profile it is made from,
    /// one paid read, and then builds the write beside that document's
    /// values and the app's calls on it, paid from the write budget. The
    /// command sees neither the HTML nor a token: only the document's status
    /// and the bundles it names, which the rediscovery walks.
    ///
    /// **The write's wait comes first.** The write budget can hold a write
    /// back for a quarter of an hour, which outlasts the browser, and the
    /// write is built only on the document it is made from, while the tab
    /// is still on it. So the wait is paid before the profile is loaded
    /// ([`crate::pace::Pacer::rested_for_a_write`]), and the write follows
    /// the document as a person's click follows the page.
    ///
    /// A document served to nobody, or to another account, ends it as an
    /// expired session before the write is asked; a document that is not a
    /// success is refused as any answer is.
    async fn write_from_the_page(
        &self,
        mutation: graphql::Mutation,
        pk: Pk,
        path: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        self.pacer.rested_for_a_write().await?;
        let ask = web::Ask::Document { path: path.into() };
        let endpoint = ask.endpoint();
        let web::Told::Document { answer, bundles } = self.ask_page(ask, "").await? else {
            return Err(told_otherwise("a document"));
        };
        let answer = answered(&endpoint, answer);
        if !answer.is_success() {
            return Err(self.refuse(&answer));
        }

        let doc_id = Self::known_doc_id(mutation, ids);
        self.rediscovering(mutation, doc_id, &bundles, ids, async |doc_id: &str| {
            self.write_asked(mutation, pk, doc_id, path).await
        })
        .await
    }

    /// One attempt at the mutation from the page: the write asked of the
    /// tab, under `doc_id`, on the profile's route, made from `referrer`.
    async fn write_asked(
        &self,
        mutation: graphql::Mutation,
        pk: Pk,
        doc_id: &str,
        referrer: &str,
    ) -> Result<FriendshipStatus, IgError> {
        let ask = web::Ask::Write {
            mutation,
            variables: graphql::variables(pk),
            doc_id: doc_id.into(),
            route: graphql::PROFILE_ROUTE.into(),
        };
        let endpoint = ask.endpoint();
        let web::Told::Answer(answer) = self.ask_page(ask, referrer).await? else {
            return Err(told_otherwise("a write"));
        };
        let answer: FriendshipResult = self.decode(&answered(&endpoint, answer))?;
        Ok(answer.status())
    }

    /// The `doc_id` a mutation is sent under first: the one found last, or
    /// the seed.
    fn known_doc_id(mutation: graphql::Mutation, ids: &dyn graphql::DocIds) -> String {
        ids.get(mutation.friendly_name())
            .unwrap_or_else(|| mutation.seed_doc_id().to_string())
    }

    /// `send` under `doc_id`, and once more under a rediscovered one only
    /// when the first answer says the write did not happen.
    ///
    /// **The recovery path.** A rotated identifier is refused, and the
    /// refusal looks like several other things — so rather than trying to
    /// read Instagram's mind, the walk runs and is worth something only if it
    /// comes back with a *different* answer. It costs megabytes off the CDN,
    /// which is why it is here and not on the way in, and the original error
    /// is what the user hears if it fails.
    async fn rediscovering(
        &self,
        mutation: graphql::Mutation,
        doc_id: String,
        bundles: &[String],
        ids: &dyn graphql::DocIds,
        send: impl AsyncFn(&str) -> Result<FriendshipStatus, IgError>,
    ) -> Result<FriendshipStatus, IgError> {
        match send(&doc_id).await {
            Ok(status) => Ok(status),
            Err(first) if worth_rediscovering(&first) => {
                let Ok(found) = self.doc_id_for(mutation, bundles, ids).await else {
                    return Err(first);
                };
                if found == doc_id {
                    return Err(first);
                }
                tracing::debug!(
                    name = mutation.friendly_name(),
                    "the stored identifier was stale; trying the new one"
                );
                send(&found).await
            }
            Err(other) => Err(other),
        }
    }

    /// One attempt at the mutation.
    async fn mutate(
        &self,
        mutation: graphql::Mutation,
        tokens: &graphql::PageTokens,
        doc_id: &str,
        pk: Pk,
        referer: &str,
    ) -> Result<FriendshipStatus, IgError> {
        let body = graphql::mutation_body(tokens, mutation, doc_id, pk);
        let form: Vec<(&str, &str)> = body.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let answer: FriendshipResult = self
            .post(
                mutation,
                &form,
                referer,
                Surface::Relay {
                    friendly_name: mutation.friendly_name(),
                    lsd: tokens.lsd.expose(),
                },
            )
            .await?;
        Ok(answer.status())
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::harness::{
        FOLLOWED, Known, LOGGED_IN_PAGE, Scripted, client, instagram_that_takes_a_write, page_said,
        spending_client, watching_as, writer,
    };
    use crate::client::page::{Method, Page, PageError, PageResponse, PushedBack};
    use crate::error::declares_failure;

    /// A second mutation is sent only after an answer that says the first
    /// did not happen.
    ///
    /// The ambiguous ones are the point: a 502 from an edge that may already
    /// have forwarded the write, the redirect neither arm follows, and a 200
    /// this program could not read. None of them may earn a discovery walk
    /// and then a second attempt on the identifier it found.
    #[test]
    fn a_write_is_rediscovered_only_after_a_refusal_that_says_it_did_not_happen() {
        let unexpected = |status: u16| IgError::Unexpected {
            status,
            body: String::new(),
        };
        for refused in [
            unexpected(400),
            unexpected(200),
            IgError::NotFound { what: None },
        ] {
            assert!(worth_rediscovering(&refused), "{refused:?}");
        }
        for ambiguous in [
            unexpected(502),
            unexpected(302),
            unexpected(0),
            IgError::Decode("an answer that would not parse".into()),
            // Instagram said not to repeat it; another identifier would be
            // refused the same way.
            IgError::QueryRefused {
                body: String::new(),
            },
            IgError::RateLimited,
            IgError::FeedbackRequired,
            IgError::SessionExpired,
            IgError::Canceled,
            IgError::NoCsrfToken,
        ] {
            assert!(
                !worth_rediscovering(&ambiguous),
                "{ambiguous:?} would have the write sent twice"
            );
        }
    }

    /// **What the mutation really answers**, captured live. Misread, the
    /// follow happens and the command says it has not.
    const FOLLOWED_GRAPHQL: &str = r#"{"data":{"xdt_create_friendship":{"friendship_status":{"following":true,"outgoing_request":false}}}}"#;

    /// The mobile shape, which this endpoint does not send and the client reads
    /// anyway. Instagram has been moving the web client onto the `/api/v1/`
    /// routes everywhere else, and the day it moves this one the answer changes
    /// shape without changing meaning.
    const FOLLOWED_OBJECT: &str =
        r#"{"status":"ok","friendship_status":{"following":true,"outgoing_request":false}}"#;

    /// **The request goes to `/api/graphql` and names the mutation**, which is
    /// what the real client does and what three live attempts established — see
    /// [`crate::graphql`] for the two that were sent first and changed nothing.
    #[tokio::test]
    async fn a_follow_names_the_mutation_with_the_id_it_was_given() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;

        let status = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();
        assert!(status.following);

        let requests = server.received_requests().await.unwrap();
        let write = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the mutation");
        assert_eq!(write.url.path(), "/api/graphql");

        let body = String::from_utf8_lossy(&write.body);
        assert!(
            body.contains("fb_api_req_friendly_name=usePolarisFollowMutation"),
            "{body}"
        );
        assert!(body.contains("doc_id=26508036048874888"), "{body}");
        assert!(body.contains("fb_dtsg=DTSG-TOKEN"), "{body}");
        assert!(body.contains("lsd=LSD-TOKEN"), "{body}");
        // The account, percent-encoded inside the variables object.
        assert!(body.contains("target_user_id"), "{body}");
    }

    /// **A page and a mutation, and both are paid for.** The page is the
    /// expensive half and it is why there is no bulk mode to be tempted by.
    #[tokio::test]
    async fn a_write_costs_a_page_and_a_mutation() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;
        writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "{requests:?}");
        // And the page asked for is the target's, which is where a browser
        // would have been standing.
        let page = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::GET)
            .expect("the page");
        assert_eq!(page.url.path(), "/someone/");
    }

    /// A page that names no viewer is nobody's, and reporting a dead session
    /// beats sending a mutation and reading whatever comes back.
    #[tokio::test]
    async fn a_page_that_names_no_viewer_is_reported_as_a_dead_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>log in</html>"))
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::SessionExpired), "{error:?}");
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method == wiremock::http::Method::GET),
            "nothing may be sent without the tokens to authorize it"
        );
    }

    /// A page served to the session's account that carries neither token
    /// cannot authorize a mutation either: the page, one GET, and nothing
    /// posted.
    #[tokio::test]
    async fn a_page_of_the_session_without_the_tokens_is_reported_as_a_dead_session() {
        let server = MockServer::start().await;
        let untokened = r#"<html><script>
    {"define":[["PolarisViewer",[],{"data":{"id":"42","username":"me","fbid":"17841400000000042"},"id":"42"},1508]]}
    </script></html>"#;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(untokened))
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::SessionExpired), "{error:?}");
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "the page alone");
        assert_eq!(received[0].method, wiremock::http::Method::GET);
    }

    /// **A logged-out page hands out both tokens**, and its `PolarisViewer`
    /// is what says so. Sending the mutation with them would be a write
    /// Instagram answers as nobody's; it is an expired session instead, and
    /// nothing is posted.
    #[tokio::test]
    async fn a_logged_out_page_with_tokens_writes_nothing() {
        const LOGGED_OUT: &str = r#"<html><script>
            {"define":[["DTSGInitData",[],{"token":"DTSG-TOKEN"},258],
                       ["LSD",[],{"token":"LSD-TOKEN"},323],
                       ["PolarisViewer",[],{"data":null,"id":null},1508]]}
            </script></html>"#;
        assert!(graphql::extract_tokens(LOGGED_OUT).is_some());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_OUT))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
            .expect(0)
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::SessionExpired), "{error:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// A page served to another account is not this session's to write
    /// from, whatever tokens it carries; and a page whose viewer was not
    /// read says nothing about whose it is.
    #[tokio::test]
    async fn a_page_of_another_account_writes_nothing() {
        const ANOTHER: &str = r#"<html><script>
            {"define":[["DTSGInitData",[],{"token":"DTSG-TOKEN"},258],
                       ["LSD",[],{"token":"LSD-TOKEN"},323],
                       ["PolarisViewer",[],{"data":{"id":"43","username":"other","fbid":"17841400000000043"},"id":"43"},1508]]}
            </script></html>"#;
        const NOBODY_SAID: &str = r#"<html><script>
            {"define":[["DTSGInitData",[],{"token":"DTSG-TOKEN"},258],
                       ["LSD",[],{"token":"LSD-TOKEN"},323]]}
            </script></html>"#;
        for page in [ANOTHER, NOBODY_SAID] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(page))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
                .expect(0)
                .mount(&server)
                .await;

            let error = writer(&server)
                .await
                .unfollow(Pk::new(7), "someone", &Known)
                .await
                .unwrap_err();
            assert!(matches!(error, IgError::SessionExpired), "{error:?}");
        }
    }

    /// The same from the page: a logged-out document the tab loaded is
    /// refused before the write is asked, or paid for.
    #[tokio::test]
    async fn a_logged_out_page_from_the_browser_writes_nothing() {
        let (client, _) = watching_as("http://127.0.0.1:9", false);
        let page = Scripted::building(|request| {
            if request.method == Method::Post {
                Ok(page_said(200, FOLLOWED))
            } else {
                Ok(page_said(
                    200,
                    &LOGGED_IN_PAGE.replace(
                        r#"{"data":{"id":"42","username":"me","fbid":"17841400000000042"},"id":"42"}"#,
                        r#"{"data":null,"id":null}"#,
                    ),
                ))
            }
        });
        let client = client.through(page.clone());

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::SessionExpired), "{error:?}");
        assert_eq!(page.asked().len(), 1, "the page, and no write");
        assert!(page.asks().iter().all(|c| !is_a_write(c)));
        assert_eq!(client.pacer().spent(), 1, "only the page was paid for");
    }

    /// Either answer shape means the same thing to the caller.
    #[tokio::test]
    async fn both_answer_shapes_read_the_same() {
        for body in [FOLLOWED, FOLLOWED_OBJECT, FOLLOWED_GRAPHQL] {
            let server = instagram_that_takes_a_write(body).await;
            let status = writer(&server)
                .await
                .follow(Pk::new(7), "someone", &Known)
                .await
                .unwrap();
            assert!(status.following, "{body}");
            assert!(!status.outgoing_request, "{body}");
        }
    }

    /// A private account answers `requested`, and that is not a follow.
    #[tokio::test]
    async fn a_private_account_answers_that_it_was_asked() {
        let server = instagram_that_takes_a_write(r#"{"result":"requested","status":"ok"}"#).await;

        let status = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();
        assert!(status.outgoing_request);
        assert!(!status.following, "a request is not a follow");
    }

    /// The three headers a POST carries that a GET does not, plus the CSRF
    /// token, which a GET carries too but a write cannot go without.
    #[tokio::test]
    async fn a_write_carries_origin_a_content_type_and_the_csrf_token() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;
        writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let headers = &requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the mutation")
            .headers;
        assert_eq!(
            headers.get("origin").unwrap(),
            server.uri().trim_end_matches('/'),
            "a same-origin POST carries Origin, unlike a same-origin GET"
        );
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(headers.get("x-csrftoken").unwrap(), "TOKEN");
        assert!(headers.contains_key("cookie"));
        // And it does announce itself as the app, because `/api/graphql` is
        // the app's route. The surface that does not is the page fetch, and
        // `a_navigation_does_not_claim_to_be_the_app` next door asserts the
        // two against each other.
        assert!(headers.contains_key("x-ig-app-id"));
        assert!(headers.contains_key("user-agent"));
        assert!(headers.contains_key("accept-language"));
    }

    /// **Nothing is sent at all** when the session cannot sign the request.
    /// Discovering it from Instagram's 403 would cost a request, a slot of the
    /// write budget, and a message telling the user their session had expired
    /// when it had not.
    #[tokio::test]
    async fn a_session_without_a_csrf_token_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
            .mount(&server)
            .await;

        // `client` builds a pasted session, which has no token.
        let error = client(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::NoCsrfToken), "{error:?}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "the refusal must happen before anything goes out"
        );
    }

    /// A redirect on a write is refused rather than followed. Following one
    /// would mean asking Instagram to do the thing a second time, and reqwest
    /// repeats the method and the body on a 307 or a 308.
    #[tokio::test]
    async fn a_redirected_write_is_not_replayed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", "/api/graphql"))
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .unfollow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 307, .. }),
            "{error:?}"
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method == wiremock::http::Method::POST)
                .count(),
            1,
            "the write must have been sent exactly once"
        );
    }

    /// An action block on a write earns a cooldown, and the budget is asked to
    /// remember it. Driven through `Recording`, which does remember, rather
    /// than through `Pacer::unlimited`, which answers `Ok(0)` and forgets.
    #[tokio::test]
    async fn an_action_block_on_a_write_is_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            // No `spam` field: that one short-circuits to `RateLimited` before
            // the message is read, and what is under test here is the action
            // block, which carries the longer cooldown.
            .and(path("/api/graphql"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"message":"feedback_required","status":"fail"}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(budget.calls().len(), 1, "the cooldown was not written down");
    }

    /// **The refusal that was being read as success.**
    ///
    /// This is the shape a real block arrives in: HTTP 200, no `status`, no
    /// `message`, the reason inside an `errors` array. Every field of
    /// `FriendshipResult` is optional, so without `declares_failure` reading
    /// this envelope the body deserializes cleanly with all of them absent and
    /// the command reports "Instagram accepted it, but nothing changed", with
    /// the write budget spent and no cooldown written down.
    #[tokio::test]
    async fn a_graphql_refusal_under_a_200_is_an_action_block_and_not_a_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":null,"errors":[{"message":"feedback_required",
                    "summary":"Try Again Later",
                    "description":"We restrict certain activity to protect our community."}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(budget.calls().len(), 1, "the cooldown was not written down");
    }

    /// The reason may be in any of the three fields, and which one is not ours
    /// to choose. `summary` alone is enough.
    #[tokio::test]
    async fn a_graphql_reason_is_found_wherever_instagram_put_it() {
        for body in [
            r#"{"errors":[{"message":"checkpoint_required"}]}"#,
            r#"{"errors":[{"summary":"checkpoint_required"}]}"#,
            r#"{"errors":[{"description":"checkpoint_required"}]}"#,
        ] {
            assert!(declares_failure(body), "not seen as a failure: {body}");
            assert!(
                matches!(
                    crate::error::classify(200, body),
                    IgError::Checkpoint { .. }
                ),
                "not classified from: {body}"
            );
        }
    }

    /// An empty `errors` array is what a *successful* GraphQL answer may carry.
    /// Reading it as a refusal would fail every write that worked.
    #[tokio::test]
    async fn an_empty_errors_array_is_not_a_refusal() {
        assert!(!declares_failure(r#"{"data":{"x":1},"errors":[]}"#));
    }

    /// **The write path measures its own push-backs now.**
    ///
    /// It was the one class of request that never did: `note_push_back` was
    /// called from `get` and from `get_body`, never from `post`, and `post` did
    /// not so much as read the header. So the endpoint most likely to say
    /// something worth hearing was the one nobody was listening to.
    #[tokio::test]
    async fn a_write_keeps_the_retry_after_it_was_given() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "30")
                    .set_body_string(r#"{"message":"please wait a few minutes"}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, _) = watching_as(&server.uri(), true);
        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
    }

    /// A 5xx the edge proxy answered in text (made-up values in the
    /// capture's shape) is the server's problem: no cooldown, and the write
    /// is not sent again, rediscovered or not.
    #[tokio::test]
    async fn a_text_server_error_on_a_write_is_neither_recorded_nor_repeated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(
                ResponseTemplate::new(500)
                    .insert_header("proxy-status", "http_request_error")
                    .set_body_raw("<html>server error</html>", "text/html"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);
        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 500, .. }),
            "{error:?}"
        );
        assert!(budget.calls().is_empty(), "{:?}", budget.calls());
        let posts = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method == wiremock::http::Method::POST)
            .count();
        assert_eq!(posts, 1);

        // The same from the page.
        let (client, budget) = watching_as("http://127.0.0.1:9", false);
        let page = page_for_a_write(|| {
            let mut answer = page_said(500, "<html>server error</html>");
            answer.headers = vec![
                ("proxy-status".into(), "http_request_error".into()),
                ("content-type".into(), "text/html".into()),
            ];
            Ok(answer)
        });
        let client = client.through(page.clone());
        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 500, .. }),
            "{error:?}"
        );
        assert!(budget.calls().is_empty(), "{:?}", budget.calls());
        assert_eq!(page.asked().len(), 2, "one page, one write, nothing more");
    }

    /// A GraphQL answer that says the call is not worth repeating stops the
    /// write where it is: no cooldown, and no second mutation.
    #[tokio::test]
    async fn a_write_refused_as_not_worth_repeating_is_sent_once() {
        let server = instagram_that_takes_a_write(
            r#"{"errors":[{"message":"invalid_variable_type","severity":"CRITICAL",
            "code":1000001,"allow_user_retry":false}],"extensions":{"is_final":true}}"#,
        )
        .await;
        let error = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::QueryRefused { .. }), "{error:?}");
        assert!(!error.is_push_back());
        let posts = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method == wiremock::http::Method::POST)
            .count();
        assert_eq!(posts, 1);
    }

    /// A tab that builds each intent on the invented page, answers the
    /// profile's document with a page served to the session's account, and
    /// the mutation with `post`. The document names no bundle, so nothing is
    /// ever downloaded.
    fn page_for_a_write(
        post: impl Fn() -> Result<PageResponse, PageError> + Send + Sync + 'static,
    ) -> std::sync::Arc<Scripted> {
        Scripted::building(move |request| {
            if request.method == Method::Post {
                post()
            } else {
                Ok(page_said(200, LOGGED_IN_PAGE))
            }
        })
    }

    /// Whether `call` asks for a write.
    fn is_a_write(call: &web::Call) -> bool {
        matches!(call.ask, web::Ask::Write { .. })
    }

    /// The fields of the app's Relay form, in its order.
    const RELAY_FIELDS: [&str; 29] = [
        "av",
        "__d",
        "__user",
        "__a",
        "__req",
        "__hs",
        "dpr",
        "__ccg",
        "__rev",
        "__s",
        "__hsi",
        "__dyn",
        "__csr",
        "__hsdp",
        "__hblp",
        "__sjsp",
        "__comet_req",
        "fb_dtsg",
        "jazoest",
        "lsd",
        "__spin_r",
        "__spin_b",
        "__spin_t",
        "__crn",
        "fb_api_caller_class",
        "fb_api_req_friendly_name",
        "server_timestamps",
        "variables",
        "doc_id",
    ];

    /// The `doc_id`s a run found since the seeds were captured: none of
    /// them the seed, so a write sent under the seed is told apart.
    struct Found;

    impl graphql::DocIds for Found {
        fn get(&self, name: &str) -> Option<String> {
            Some(match name {
                "usePolarisFollowMutation" => "11111111111111111".into(),
                _ => "22222222222222222".into(),
            })
        }
        fn put(&self, _: &str, _: &str) {}
    }

    /// From the page, a write is the profile's document, one read, and the
    /// write the tab builds beside it, one write: the app's 29 fields in its
    /// order, as the viewer's fbid, on the profile's route, under the
    /// `doc_id` the run knows for it rather than the seed, made from the
    /// profile. The session needs no token of its own: the page has one.
    #[tokio::test]
    async fn a_write_from_the_page_is_built_in_the_tab_with_the_apps_form() {
        let page = page_for_a_write(|| Ok(page_said(200, FOLLOWED)));
        let (client, budget) = spending_client(page.clone());

        let status = client
            .follow(Pk::new(7), "someone", &Found)
            .await
            .expect("the page took the write");
        assert!(status.following);
        assert_eq!(budget.reserved(), (1, 1), "one read and one write");
        assert_eq!(
            budget.order(),
            ["write wait", "read", "write"],
            "the write's wait before the profile, and none between the two"
        );

        let asks = page.asks();
        assert_eq!(asks.len(), 2, "the document and the write");
        assert_eq!(
            asks[0].ask,
            web::Ask::Document {
                path: "/someone/".into()
            }
        );
        assert_eq!(
            asks[1].ask,
            web::Ask::Write {
                mutation: graphql::Mutation::Follow,
                variables: graphql::variables(Pk::new(7)),
                doc_id: "11111111111111111".into(),
                route: graphql::PROFILE_ROUTE.into(),
            }
        );
        assert_eq!(asks[1].referrer, "/someone/");

        let asked = page.asked();
        assert_eq!(asked.len(), 2, "the page and the mutation");
        assert!(asked[0].navigate, "the profile is loaded");
        let write = &asked[1];
        assert_eq!(write.method, Method::Post);
        assert_eq!(write.url, "http://127.0.0.1:9/api/graphql");
        assert_eq!(write.referrer, "http://127.0.0.1:9/someone/");
        let form: Vec<(String, String)> =
            url::form_urlencoded::parse(write.body.as_deref().unwrap_or_default().as_bytes())
                .into_owned()
                .collect();
        let names: Vec<&str> = form.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, RELAY_FIELDS);
        let field = |name: &str| {
            form.iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("av"), Some("17841400000000001"), "the viewer's fbid");
        assert_eq!(field("__crn"), Some(graphql::PROFILE_ROUTE));
        assert_eq!(field("doc_id"), Some("11111111111111111"), "the one found");
        assert_eq!(
            field("fb_api_req_friendly_name"),
            Some("usePolarisFollowMutation")
        );
        assert_eq!(field("fb_dtsg"), Some("relay:token"));
        assert_eq!(field("__req"), Some("e"), "after the app's own");
    }

    /// With a page, a write is asked of the tab and never sent as a request
    /// built here: `post` refuses before it pays, and nothing reaches the
    /// server or the page.
    #[tokio::test]
    async fn post_refuses_a_write_built_outside_the_tab() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;
        let page = page_for_a_write(|| Ok(page_said(200, FOLLOWED)));
        let client = writer(&server).await.through(page.clone());
        let mutation = graphql::Mutation::Follow;

        let error = client
            .post::<FriendshipResult>(
                mutation,
                &[("doc_id", mutation.seed_doc_id())],
                "someone/",
                Surface::Relay {
                    friendly_name: mutation.friendly_name(),
                    lsd: "LSD-TOKEN",
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, IgError::NotAllowed(what) if what.ends_with("built outside the tab")),
            "{error:?}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(page.asked().is_empty() && page.asks().is_empty());
        assert_eq!(client.pacer().spent(), 0, "nothing paid");
    }

    /// A stale `doc_id` refused with no bundle to look in ends with the
    /// first refusal, after exactly one write, and nothing downloaded.
    #[tokio::test]
    async fn a_stale_id_with_no_bundle_to_look_in_is_sent_once() {
        let page = page_for_a_write(|| {
            Ok(page_said(
                400,
                r#"{"message":"a made-up refusal","status":"fail"}"#,
            ))
        });
        let (client, budget) = spending_client(page.clone());

        let error = client
            .unfollow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 400, .. }),
            "{error:?}"
        );
        assert_eq!(page.asks().iter().filter(|c| is_a_write(c)).count(), 1);
        assert_eq!(page.asked().len(), 2, "one page, one write, nothing more");
        assert_eq!(budget.reserved(), (1, 1));
    }

    /// The walk opens only what the fixed bundle rule takes, whatever list
    /// it is handed: an address on a test server is dropped without being
    /// asked, and the mutation is not found.
    #[tokio::test]
    async fn a_bundle_off_the_cdn_is_never_downloaded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{name:"usePolarisFollowMutation",id:"12345678901234567"}"#),
            )
            .expect(0)
            .mount(&server)
            .await;
        let bundles = [
            format!("{}/rsrc.php/v4/a.js", server.uri()),
            format!("{}/static.cdninstagram.com/b.js", server.uri()),
        ];

        let error = writer(&server)
            .await
            .doc_id_for(graphql::Mutation::Follow, &bundles, &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::MutationNotFound { .. }),
            "{error:?}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// A write the page met a redirect on arrives as status 0, and is refused
    /// with nothing sent after it: not followed, not rediscovered, not sent
    /// again.
    #[tokio::test]
    async fn a_redirected_write_from_the_page_is_not_replayed() {
        let (client, _) = watching_as("http://127.0.0.1:9", false);
        let page = page_for_a_write(|| Ok(page_said(0, "")));
        let client = client.through(page.clone());

        let error = client
            .unfollow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 0, .. }),
            "{error:?}"
        );
        assert_eq!(page.asked().len(), 2, "one page, one write, nothing more");
    }

    /// A tab that loads the profile's document, and has heard a push-back
    /// on the app's own calls by then.
    struct HeardAfterThePage {
        page: std::sync::Arc<Scripted>,
    }

    impl Page for HeardAfterThePage {
        fn send(
            &self,
            request: crate::client::page::PageRequest,
        ) -> crate::client::page::PageFuture<'_> {
            self.page.send(request)
        }
        fn ask(&self, call: web::Call) -> crate::client::page::AskFuture<'_> {
            self.page.ask(call)
        }
        fn heard(&self) -> Option<PushedBack> {
            (!self.page.asked().is_empty()).then_some(PushedBack::FeedbackRequired)
        }
    }

    /// A push-back the browser heard while the profile was loading stops
    /// the write before it is paid for, and above all before it is sent; it
    /// was recorded where it was heard, and is not recorded again here.
    #[tokio::test]
    async fn a_push_back_heard_before_the_write_stops_it_unpaid() {
        let (client, budget) = watching_as("http://127.0.0.1:9", false);
        let page = page_for_a_write(|| Ok(page_said(200, FOLLOWED)));
        let client = client.through(std::sync::Arc::new(HeardAfterThePage {
            page: page.clone(),
        }));

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(page.asked().len(), 1, "the page, and no write");
        assert_eq!(client.pacer().spent(), 1, "only the page was paid for");
        assert!(
            budget.calls().is_empty(),
            "recorded again: {:?}",
            budget.calls()
        );
    }

    /// A push-back the page answers a write with is its cause, and it was
    /// recorded where it was heard: never here as well, which would double
    /// the twelve hours an action block earns.
    #[tokio::test]
    async fn a_push_back_the_page_answers_a_write_with_is_never_recorded_again() {
        let (client, budget) = watching_as("http://127.0.0.1:9", false);
        let page = page_for_a_write(|| Err(PageError::PushedBack(PushedBack::FeedbackRequired)));
        let client = client.through(page.clone());

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(page.asked().len(), 2, "one page, one write, nothing more");
        assert!(
            budget.calls().is_empty(),
            "recorded again: {:?}",
            budget.calls()
        );
    }

    /// A browser with no CSRF token to write with says so, as the session
    /// without one does.
    #[tokio::test]
    async fn a_page_with_no_csrf_token_is_told_apart() {
        let (client, _) = watching_as("http://127.0.0.1:9", false);
        let page = page_for_a_write(|| Err(PageError::NoCsrfToken));
        let client = client.through(page);

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::NoCsrfToken), "{error:?}");
    }
}
