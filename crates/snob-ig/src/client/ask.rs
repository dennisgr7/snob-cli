//! Intents sent to the browser tab, and what they are paid with.
//!
//! A command names what it wants of the tab with a [`web::Ask`], and the
//! process holding the browser builds the request from the page's values
//! right before it sends it; the client never sees a token. What it keeps
//! is everything around the hop, as `IgClient::get` keeps it for a
//! request it builds itself: the push-back heard first, the payment, the
//! cancel, and the answer read the way an answer off the wire is.
//!
//! **The ask decides what is paid, never the caller.** What the tab answers
//! from its document sends nothing and costs nothing, though a cooldown or a
//! Ctrl+C still stops it; a write pays the write bucket and is not abandoned
//! once it is in flight, as `IgClient::post`'s is not; everything else is
//! one read, given up the moment the user asks.

use snob_core::Pk;

use crate::allowlist::{Operation, Rest};
use crate::error::IgError;
use crate::model::web::{
    GraphUser, HighlightsTrayPage, HoverCard, ProfilePage, ReelsPage, RouteAnswer,
};
use crate::model::{Counters, Highlight, Reel, WebProfileInfo};
use crate::web::{Ask, Call, Told};

use super::IgClient;
use super::page::PageResponse;
use super::transport::{Answer, MAX_BODY_BYTES, REQUEST_TIMEOUT};

impl IgClient {
    /// Asks the tab for `ask`, made from `referrer`, a path such as
    /// `/<user>/`. Needs a page: the `reqwest` path has none to ask.
    ///
    /// An answer that is a request's, an [`Told::Answer`], [`Told::Pk`] or
    /// [`Told::Document`], has been read by `IgClient::answer_from_page`:
    /// its hops paid for, its claim kept, and an answer too large refused.
    /// What it says is the caller's to decode.
    pub(crate) async fn ask_page(&self, ask: Ask, referrer: &str) -> Result<Told, IgError> {
        let Some(page) = self.page.as_deref() else {
            return Err(IgError::Browser(
                "this client sends without a browser, which has no page to ask".into(),
            ));
        };
        // Before paying: an intent snob may not send costs nothing, and a
        // push-back the browser heard stops this with its cause, and is not
        // written down again.
        if let Some(what) = crate::allowlist::refused_ask(&ask) {
            tracing::error!(request = %what, "refused a call snob may not send");
            return Err(IgError::NotAllowed(what));
        }
        if let Some(cause) = page.heard() {
            return Err(cause.into());
        }
        let write = matches!(ask, Ask::Write { .. });
        match ask {
            Ask::Viewer | Ask::Tray => {
                if self.pacer.cancel_token().is_canceled() {
                    return Err(IgError::Canceled);
                }
                if let Some(until_ms) = self.pacer.cooldown_off_thread().await? {
                    return Err(IgError::InCooldown { until_ms });
                }
            }
            // Nothing is paid: the CDN is not paced and not Instagram's API,
            // and a cooldown on the account is about the API. A push-back the
            // browser heard has stopped it above, and Ctrl+C stops it here.
            Ask::Asset { .. } => {
                if self.pacer.cancel_token().is_canceled() {
                    return Err(IgError::Canceled);
                }
            }
            Ask::Write { .. } => {
                // Before the budget is charged, as `post` refuses: a session
                // that cannot write does not spend a slot finding out.
                if !self.can_write() {
                    return Err(IgError::NoCsrfToken);
                }
                self.pacer.clear_to_send_write().await?;
            }
            _ => self.pacer.clear_to_send().await?,
        }

        let asked = self.base.join(&ask.endpoint())?;
        tracing::debug!(url = %asked, ask = ask_name(&ask), "asked of the page");
        let cap = match ask {
            Ask::Asset { .. } => super::media::PAGE_ASSET_BYTES as u64,
            _ => MAX_BODY_BYTES,
        };
        let call = Call {
            ask,
            origin: self.base.as_str().trim_end_matches('/').to_string(),
            referrer: referrer.to_string(),
            claim: self.claim.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            cap,
            timeout_ms: REQUEST_TIMEOUT.as_millis() as u64,
        };
        let told = if write {
            // A write in flight is the one request Ctrl+C does not abandon.
            page.ask(call).await.map_err(IgError::from)?
        } else {
            tokio::select! {
                biased;
                () = self.pacer.cancel_token().canceled() => return Err(IgError::Canceled),
                told = page.ask(call) => told.map_err(IgError::from)?,
            }
        };

        Ok(match told {
            Told::Answer(answer) => Told::Answer(self.read_told(&asked, answer, write).await?),
            Told::Pk { answer, pk } => Told::Pk {
                answer: self.read_told(&asked, answer, write).await?,
                pk,
            },
            Told::Document { answer, bundles } => Told::Document {
                answer: self.read_told(&asked, answer, write).await?,
                bundles,
            },
            told @ (Told::Viewer(_) | Told::Tray(_) | Told::Asset(_)) => told,
        })
    }

    /// A request's answer, read the way `IgClient::get` reads one from the
    /// page: an opaque redirect is refused with nothing more sent, and the
    /// rest goes through `IgClient::answer_from_page`. Handed back whole,
    /// its body the one that was read.
    async fn read_told(
        &self,
        asked: &url::Url,
        mut response: PageResponse,
        write: bool,
    ) -> Result<PageResponse, IgError> {
        // An opaque redirect: Instagram pointed somewhere and the page did
        // not go. `Unexpected` with no status is an `Abort` everywhere it is
        // read, as for a GET or a write sent from the page.
        if response.status == 0 && !response.redirected {
            let what = if write {
                "Instagram redirected a write, which is never followed"
            } else {
                "Instagram answered with a redirect, which is not followed"
            };
            return Err(IgError::Unexpected {
                status: 0,
                body: what.into(),
            });
        }
        let body = std::mem::take(&mut response.body);
        let mut read = response.clone();
        response.body = body;
        read.body = self.answer_from_page(asked, response).await?.body;
        Ok(read)
    }
}

impl IgClient {
    /// The pk behind `name`, as the tab finds it: the route definitions, or
    /// the profile document when the app has made no route call to copy.
    /// One read either way. An answer that is not a success is refused as
    /// any other is, a push-back recorded; what the route says is the
    /// caller's to read.
    pub(crate) async fn pk_of(&self, name: &str) -> Result<RouteAnswer, IgError> {
        let ask = Ask::Pk { name: name.into() };
        let endpoint = ask.endpoint();
        match self.ask_page(ask, "/").await? {
            Told::Pk { answer, pk } => {
                let answer = answered(&endpoint, answer);
                if !answer.is_success() {
                    return Err(self.refuse(&answer));
                }
                Ok(pk)
            }
            _ => Err(told_otherwise("a name")),
        }
    }

    /// The profile of `pk`, as the web client reads it: its profile query,
    /// made from the page of `name` (the home page when the name is not
    /// known). The query carries the app's seven variables, the capture's
    /// ([`crate::web::profile_variables`]); the tab replaces the flags with
    /// the ones the app sent with its own query, where it has seen one, with
    /// only the id made this one.
    ///
    /// An answer that names nobody is not found.
    ///
    /// The query alone: a profile accepted as the one asked for is opened
    /// with the reads the app sends beside it ([`Self::profile_burst`]), by
    /// the caller that accepts it.
    pub(crate) async fn profile_by_pk(
        &self,
        pk: Pk,
        name: &str,
    ) -> Result<WebProfileInfo, IgError> {
        let operation = Operation::ProfilePage;
        let referrer = profile_path(name);
        let ask = Ask::Query {
            operation,
            variables: crate::web::profile_variables(pk),
        };
        let endpoint = ask.endpoint();
        let Told::Answer(answer) = self.ask_page(ask, &referrer).await? else {
            return Err(told_otherwise("a profile"));
        };
        let page: ProfilePage = self.decode_query(
            &answered(&endpoint, answer),
            Operation::ProfilePage.root_field(),
        )?;
        page.profile().ok_or_else(|| IgError::NotFound {
            what: (!name.is_empty()).then(|| name.to_string()),
        })
    }

    /// The reads the app sends with every profile it opens, beside its
    /// profile query: the highlights tray, the note over the picture and the
    /// school badge, each by pk, made from the same page. In the capture of
    /// 2026-10-01 all three went out within about a second of the profile
    /// query on every one of the ten profiles opened (with the posts and the
    /// suggested accounts, which the app sends on a profile's own document and
    /// snob does not); a profile asked alone, ten times over, is a client that
    /// is not the app. Sent one after the other, a moment apart, as the tab
    /// answers each.
    ///
    /// **Each is paid as the read it is**: opening a profile costs four reads
    /// of the request budget, not one. The tray's answer is kept for the
    /// command that shows it ([`Self::take_held_tray`]), so `profile` and
    /// `highlights` do not ask for it a second time; the note's and the
    /// badge's are read for a push-back and nothing else.
    ///
    /// None of them is the answer asked for, so one that fails is said in the
    /// log and passed over, unless the failure has to stop everything
    /// ([`must_stop`]): a push-back, a dead session, a Ctrl+C.
    pub(crate) async fn profile_burst(&self, pk: Pk, name: &str) -> Result<(), IgError> {
        match self.highlights_tray_by_pk(pk, name).await {
            Ok(tray) => {
                *self.held_tray.lock().unwrap_or_else(|e| e.into_inner()) = Some(HeldTray {
                    pk,
                    action: self.pacer.actions(),
                    tray,
                });
            }
            Err(e) => passed_over(e, "the highlights tray of a profile opened")?,
        }
        let referrer = profile_path(name);
        for (operation, variables) in [
            (Operation::NoteBubble, format!(r#"{{"user_id":"{pk}"}}"#)),
            (
                Operation::SchoolPartnerBadge,
                format!(r#"{{"igid":"{pk}"}}"#),
            ),
        ] {
            let ask = Ask::Query {
                operation,
                variables,
            };
            let endpoint = ask.endpoint();
            let read = match self.ask_page(ask, &referrer).await {
                Ok(Told::Answer(answer)) => self
                    .decode::<serde_json::Value>(&answered(&endpoint, answer))
                    .map(drop),
                Ok(_) => Err(told_otherwise(operation.friendly_name())),
                Err(e) => Err(e),
            };
            if let Err(e) = read {
                passed_over(e, operation.friendly_name())?;
            }
        }
        Ok(())
    }

    /// The highlights tray of `pk` the profile opened in this action was
    /// read with, taken: once, and only within the action it was read in,
    /// as anything read is trusted for (`Pacer::begin_action`).
    pub(crate) fn take_held_tray(&self, pk: Pk) -> Option<Vec<Highlight>> {
        let mut held = self.held_tray.lock().unwrap_or_else(|e| e.into_inner());
        match held.take() {
            Some(tray) if tray.pk == pk && tray.action == self.pacer.actions() => Some(tray.tray),
            _ => None,
        }
    }

    /// The app's router moving to `route` ([`Ask::Navigation`]), as it does
    /// when a profile opens and when its mutual followers open: paid as one
    /// read, and its answer, the route's definition, read for a push-back
    /// and nothing else. Its body is the app's `for (;;);` envelope, not
    /// JSON, so only its status is read.
    ///
    /// A navigation the tab cannot build yet (the app has made no route call
    /// on the document whose envelope it copies) or that fails without a
    /// push-back is said in the log and passed over: the list is read either
    /// way.
    pub(crate) async fn navigation(&self, route: &str) -> Result<(), IgError> {
        let ask = Ask::Navigation {
            route: route.to_string(),
        };
        let endpoint = ask.endpoint();
        let sent = match self.ask_page(ask, route).await {
            Ok(Told::Answer(answer)) => {
                let answer = answered(&endpoint, answer);
                if answer.is_success() {
                    Ok(())
                } else {
                    Err(self.refuse(&answer))
                }
            }
            Ok(_) => Err(told_otherwise("a navigation")),
            Err(e) => Err(e),
        };
        sent.or_else(|e| passed_over(e, "a navigation"))
    }

    /// How the viewer stands with `users`, the accounts of a list page just
    /// read from `referrer` ([`Ask::Statuses`], the app's
    /// `friendships/show_many`): one read, sent the moment the page answers
    /// and before the next is asked, as the app sends it. Its answer is read
    /// for a push-back and otherwise discarded: the lists carry what snob
    /// needs, and the one flag of its own (`following`, on the viewer's own
    /// followers) is already known from the viewer's following.
    ///
    /// **Sent only with the token the app's REST POSTs carry**, which the
    /// page is not known to show (`web::AppCalls::rest_token_sent`). When the
    /// tab says it has not seen one, it is said once in the log and no other
    /// is asked for by this client, so the reservation paid for the one not
    /// sent is the only one. A page with no accounts asks nothing. Anything
    /// else that fails without a push-back is passed over: the list page has
    /// been read.
    pub(crate) async fn statuses(&self, users: &[Pk], referrer: &str) -> Result<(), IgError> {
        use std::sync::atomic::Ordering;

        if users.is_empty() || self.statuses_unbuilt.load(Ordering::Relaxed) {
            return Ok(());
        }
        let pks = users
            .iter()
            .copied()
            .take(crate::allowlist::MOST_STATUSES)
            .collect();
        let ask = Ask::Statuses { pks };
        let endpoint = ask.endpoint();
        let sent = match self.ask_page(ask, referrer).await {
            Ok(Told::Answer(answer)) => self
                .decode::<serde_json::Value>(&answered(&endpoint, answer))
                .map(drop),
            Ok(_) => Err(told_otherwise("the statuses of a list page")),
            Err(IgError::PageNotReady(what)) => {
                tracing::debug!(
                    missing = %what,
                    "show_many is not sent: the app has sent no REST token to send it with"
                );
                self.statuses_unbuilt.store(true, Ordering::Relaxed);
                return Ok(());
            }
            Err(e) => Err(e),
        };
        sent.or_else(|e| passed_over(e, "show_many"))
    }
}

/// The highlights tray a profile was opened with, kept for the command
/// that shows it ([`IgClient::take_held_tray`]).
#[derive(Debug)]
pub(super) struct HeldTray {
    pk: Pk,
    /// `Pacer::actions` when it was read.
    action: u32,
    tray: Vec<Highlight>,
}

/// Whether `error`, on a call sent beside the one asked for (the reads that
/// open a profile, a navigation, a `show_many`), has to end what is going
/// on: Instagram objected to the account, the session is dead, the person
/// asked to stop, or snob tried to send what it may not. Anything else is
/// that call's alone.
fn must_stop(error: &IgError) -> bool {
    error.is_push_back()
        || error.invalidates_session()
        || matches!(
            error,
            IgError::Canceled | IgError::NotAllowed(_) | IgError::Budget(_)
        )
}

/// `error`, on the call sent beside the one asked for that `what` names,
/// handed on when it [`must_stop`] and otherwise said in the log and passed
/// over.
fn passed_over(error: IgError, what: &str) -> Result<(), IgError> {
    if must_stop(&error) {
        return Err(error);
    }
    tracing::debug!(error = %error, call = what, "a call sent alongside failed; passed over");
    Ok(())
}

impl IgClient {
    /// The two counters of `pk`, from the card the web client shows over an
    /// account's name: its hover-card query, by pk, made from the home page.
    /// One read, and a small answer, which carries the counters of a private
    /// account too.
    ///
    /// `None` when the answer names nobody.
    pub(crate) async fn hover_card(&self, pk: Pk) -> Result<Option<Counters>, IgError> {
        let ask = Ask::Query {
            operation: Operation::HoverCard,
            variables: format!(r#"{{"userID":"{pk}"}}"#),
        };
        let endpoint = ask.endpoint();
        let Told::Answer(answer) = self.ask_page(ask, "/").await? else {
            return Err(told_otherwise("a hover card"));
        };
        let card: HoverCard = self.decode_query(
            &answered(&endpoint, answer),
            Operation::HoverCard.root_field(),
        )?;
        Ok(card.user().map(GraphUser::counters))
    }
}

impl IgClient {
    /// The highlights of `pk` without their items, as the web client reads
    /// them on every profile it opens: its tray query, by pk, made from the
    /// page of `name`. One read. The tray carries no item count and no
    /// dates, so those stay unknown.
    pub(crate) async fn highlights_tray_by_pk(
        &self,
        pk: Pk,
        name: &str,
    ) -> Result<Vec<Highlight>, IgError> {
        let ask = Ask::Query {
            operation: Operation::HighlightsTray,
            variables: format!(r#"{{"user_id":"{pk}"}}"#),
        };
        let endpoint = ask.endpoint();
        let Told::Answer(answer) = self.ask_page(ask, &profile_path(name)).await? else {
            return Err(told_otherwise("a highlights tray"));
        };
        let page: HighlightsTrayPage = self.decode_query(
            &answered(&endpoint, answer),
            Operation::HighlightsTray.root_field(),
        )?;
        Ok(page.tray().tray)
    }

    /// The items of highlight `id`, as the web client's highlights viewer
    /// reads them: its page query, opened at `id`, with every highlight of
    /// `tray` in the tray's order and a window of three
    /// after and two before, and the flag the viewer adds, made from the page
    /// of `name`. One read. Only the reel whose id is `id` is kept.
    ///
    /// **Nothing is marked as seen**: the viewer marks what it shows with a
    /// mutation of its own, and the tab never opens it; this is the read
    /// alone.
    ///
    /// Its `X-Root-Field-Name` is the capture's own
    /// (`Operation::root_field`). An answer under another field fails as one
    /// that does not decode, rather than read as an empty highlight.
    pub(crate) async fn highlight_window(
        &self,
        id: &str,
        tray: &[String],
        name: &str,
    ) -> Result<Option<Reel>, IgError> {
        let ask = Ask::Query {
            operation: Operation::HighlightsPage,
            variables: crate::web::reel_variables(id, tray),
        };
        let endpoint = ask.endpoint();
        let Told::Answer(answer) = self.ask_page(ask, &profile_path(name)).await? else {
            return Err(told_otherwise("a highlight"));
        };
        let page: ReelsPage = self.decode_query(
            &answered(&endpoint, answer),
            Operation::HighlightsPage.root_field(),
        )?;
        Ok(page
            .highlights()
            .into_iter()
            .find(|reel| reel.id.as_deref() == Some(id)))
    }
}

impl IgClient {
    /// The ids of the reels in the stories tray the tab's document shows,
    /// in its order: what the home page preloads, read off the document for
    /// nothing. `None` when the document holds no tray, as a profile's does
    /// not.
    pub(crate) async fn tray(&self) -> Result<Option<Vec<String>>, IgError> {
        match self.ask_page(Ask::Tray, "/").await? {
            Told::Tray(ids) => Ok(ids),
            _ => Err(told_otherwise("the stories tray")),
        }
    }

    /// The stories of `pk`, as the web client's story viewer reads them:
    /// its gallery query, opened at `pk`, with `reel_ids` the reels of the
    /// tray it was opened from, in its order, and a window of three after
    /// and two before, and the flag the viewer adds, made from the home
    /// page. One read. Only the reel whose id is `pk` is kept, and it comes
    /// whole in the first answer, so the gallery's next page is never asked.
    ///
    /// **Nothing is marked as seen**: the viewer marks what it shows with a
    /// mutation of its own, and the tab never opens it; this is the read
    /// alone.
    pub(crate) async fn reel_gallery(
        &self,
        pk: Pk,
        reel_ids: &[String],
    ) -> Result<Option<Reel>, IgError> {
        let id = pk.to_string();
        let ask = Ask::Query {
            operation: Operation::ReelGallery,
            variables: crate::web::reel_variables(&id, reel_ids),
        };
        let endpoint = ask.endpoint();
        let Told::Answer(answer) = self.ask_page(ask, "/").await? else {
            return Err(told_otherwise("a reel"));
        };
        let page: ReelsPage = self.decode_query(
            &answered(&endpoint, answer),
            Operation::ReelGallery.root_field(),
        )?;
        Ok(page
            .reels()
            .into_iter()
            .find(|reel| reel.id.as_deref() == Some(id.as_str())))
    }
}

impl IgClient {
    /// A REST read of the registry's, `query` in the order the app sends
    /// it, built by the tab with the app's headers and made from `referrer`.
    pub(crate) async fn rest_read<T: serde::de::DeserializeOwned>(
        &self,
        read: Rest,
        query: &[(&str, &str)],
        referrer: &str,
    ) -> Result<T, IgError> {
        let ask = Ask::Rest {
            read,
            query: query
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let endpoint = ask.endpoint();
        let Told::Answer(answer) = self.ask_page(ask, referrer).await? else {
            return Err(told_otherwise("a list"));
        };
        self.decode(&answered(&endpoint, answer))
    }
}

/// The page of the account called `name`, `/<name>/`, which is where the
/// web client asks about it from; the home page when the name is not
/// known.
pub(super) fn profile_path(name: &str) -> String {
    if name.is_empty() {
        "/".to_string()
    } else {
        format!("/{}/", snob_core::model::in_a_path(name))
    }
}

/// A page's answer, as the client reads one off the wire: for
/// `IgClient::decode` and `IgClient::refuse`.
pub(super) fn answered(endpoint: &str, response: PageResponse) -> Answer {
    Answer {
        endpoint: endpoint.to_string(),
        status: response.status,
        retry_after: response.header("retry-after").map(str::to_string),
        load: response.load(),
        served: response.served(),
        body: response.body,
    }
}

/// The tab answered an ask with another kind of answer.
pub(super) fn told_otherwise(what: &str) -> IgError {
    IgError::Unexpected {
        status: 0,
        body: format!("the page was asked for {what} and answered with something else"),
    }
}

/// The kind of ask, for a log line: never what it carries.
fn ask_name(ask: &Ask) -> &'static str {
    match ask {
        Ask::Viewer => "viewer",
        Ask::Tray => "tray",
        Ask::Pk { .. } => "pk",
        Ask::Document { .. } => "document",
        Ask::Query { operation, .. } => operation.friendly_name(),
        Ask::Rest { .. } => "rest",
        Ask::Navigation { .. } => "navigation",
        Ask::Statuses { .. } => "show_many",
        Ask::Write { mutation, .. } => mutation.friendly_name(),
        Ask::Asset { .. } => "asset",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use snob_core::Pk;

    use super::*;
    use crate::allowlist::{Operation, Rest, refused_ask};
    use crate::client::harness::{Scripted, page_said, spending_client};
    use crate::client::page::{AskFuture, Page, PageError, PageFuture, PageRequest};
    use crate::graphql::Mutation;
    use crate::model::web::RouteAnswer;
    use crate::pace::Pacer;
    use crate::page_values::Viewer;
    use crate::web::{Missing, Unbuilt};

    fn viewer() -> Told {
        Told::Viewer(Viewer {
            pk: Pk::new(42),
            username: "me".into(),
            fbid: "17841400000000042".into(),
        })
    }

    fn query() -> Ask {
        Ask::Query {
            operation: Operation::HoverCard,
            variables: r#"{"userID":"9001"}"#.into(),
        }
    }

    fn follow() -> Ask {
        Ask::Write {
            mutation: Mutation::Follow,
            variables: r#"{"target_user_id":"9001"}"#.into(),
            doc_id: "1000000000000003".into(),
            route: "comet.igweb.PolarisProfilePostsTabRoute".into(),
        }
    }

    /// What the tab answers itself costs nothing, and still waits out a
    /// cooldown without asking.
    #[tokio::test]
    async fn the_viewer_is_free_and_waits_out_a_cooldown() {
        let page = Scripted::telling(|_| Ok(viewer()));
        let (client, budget) = spending_client(page.clone());
        let told = client.ask_page(Ask::Viewer, "/").await.unwrap();
        assert!(matches!(told, Told::Viewer(v) if v.pk == Pk::new(42)));
        assert_eq!(client.pacer().spent(), 0);
        assert_eq!(budget.reserved(), (0, 0));
        assert_eq!(page.asks().len(), 1);

        budget.cool_down();
        let error = client.ask_page(Ask::Viewer, "/").await.unwrap_err();
        assert!(matches!(error, IgError::InCooldown { .. }), "{error:?}");
        let error = client.ask_page(Ask::Tray, "/").await.unwrap_err();
        assert!(matches!(error, IgError::InCooldown { .. }), "{error:?}");
        assert_eq!(page.asks().len(), 1, "nothing asked in a cooldown");
    }

    /// A read is one read, and a Ctrl+C gives it up.
    #[tokio::test]
    async fn a_query_spends_one_read_and_stops_on_ctrl_c() {
        let page = Scripted::telling(|_| Ok(Told::Answer(page_said(200, "{}"))));
        let (client, budget) = spending_client(page.clone());
        let told = client.ask_page(query(), "/someone/").await.unwrap();
        assert!(matches!(told, Told::Answer(ref a) if a.body == "{}"));
        assert_eq!(budget.reserved(), (1, 0));
        let call = &page.asks()[0];
        assert_eq!(call.ask, query());
        assert_eq!(call.origin, "http://127.0.0.1:9");
        assert_eq!(call.referrer, "/someone/");
        assert_eq!(call.claim, "0");
        assert_eq!(call.cap, MAX_BODY_BYTES);
        assert_eq!(call.timeout_ms, 60_000);

        // Canceled before: nothing is paid for or asked.
        client.pacer().cancel_token().cancel();
        let error = client.ask_page(query(), "/").await.unwrap_err();
        assert!(matches!(error, IgError::Canceled), "{error:?}");
        assert_eq!(budget.reserved(), (1, 0));
        assert_eq!(page.asks().len(), 1);
    }

    /// Ctrl+C while a read is in flight gives it up at once.
    #[tokio::test]
    async fn a_read_in_flight_is_given_up() {
        let (client, _) = spending_client(Arc::new(Stalled));
        let cancel = client.pacer().cancel_token().clone();
        let asked = tokio::spawn(async move { client.ask_page(query(), "/").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let error = asked.await.unwrap().unwrap_err();
        assert!(matches!(error, IgError::Canceled), "{error:?}");
    }

    /// A write pays the write bucket, and a Ctrl+C once it is in flight does
    /// not abandon it.
    #[tokio::test]
    async fn a_write_spends_one_write_and_is_not_abandoned() {
        let (client, budget) = spending_client(Arc::new(Slow));
        let cancel = client.pacer().cancel_token().clone();
        let asked = tokio::spawn(async move { client.ask_page(follow(), "/someone/").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let told = asked.await.unwrap().unwrap();
        assert!(matches!(told, Told::Answer(ref a) if a.status == 200));
        assert_eq!(budget.reserved(), (0, 1));
    }

    /// An intent the allowlist refuses never reaches the page, and costs
    /// nothing.
    #[tokio::test]
    async fn a_refused_ask_never_reaches_the_page() {
        let page = Scripted::telling(|_| Ok(Told::Answer(page_said(200, "{}"))));
        let (client, budget) = spending_client(page.clone());
        let pk = Pk::new(9001);
        for ask in [
            Ask::Query {
                operation: Operation::Follow,
                variables: "{}".into(),
            },
            Ask::Pk {
                name: "stories/someone".into(),
            },
            Ask::Document {
                path: "/explore/".into(),
            },
            Ask::Rest {
                read: Rest::Following(pk),
                query: vec![("search_surface".into(), "follow_list_page".into())],
            },
        ] {
            assert!(refused_ask(&ask).is_some(), "{ask:?}");
            let error = client.ask_page(ask, "/").await.unwrap_err();
            assert!(matches!(error, IgError::NotAllowed(_)), "{error:?}");
        }
        assert!(page.asks().is_empty());
        assert_eq!(budget.reserved(), (0, 0));
    }

    /// What the tab could not build maps to what the command tells: a page
    /// not ready is not a dead browser, and a page logged out is a dead
    /// session.
    #[tokio::test]
    async fn a_page_not_ready_or_logged_out_is_told_as_such() {
        let page = Scripted::telling(|call| match call.ask {
            Ask::Query { .. } => Err(PageError::NotReady("a Relay call of the app's".into())),
            _ => Err(PageError::LoggedOut),
        });
        let (client, _) = spending_client(page);
        let error = client.ask_page(query(), "/").await.unwrap_err();
        assert!(
            matches!(&error, IgError::PageNotReady(what) if what == "a Relay call of the app's"),
            "{error:?}"
        );
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
        assert!(error.to_string().contains("nothing was sent"), "{error}");
        let error = client.ask_page(Ask::Viewer, "/").await.unwrap_err();
        assert!(matches!(error, IgError::SessionExpired), "{error:?}");

        assert!(matches!(
            PageError::from(Unbuilt::Missing(Missing::RELAY_CALL)),
            PageError::NotReady(what) if what == "a Relay call of the app's"
        ));
        assert!(matches!(
            PageError::from(Unbuilt::LoggedOut),
            PageError::LoggedOut
        ));
        assert!(matches!(
            PageError::from(Unbuilt::NotAllowed("POST /x".into())),
            PageError::NotAllowed(what) if what == "POST /x"
        ));
    }

    /// The claim an answer hands out is the one the next ask carries.
    #[tokio::test]
    async fn the_claim_a_told_answer_sets_is_sent_next() {
        let page = Scripted::telling(|call| {
            let mut answer = page_said(200, "{}");
            answer.headers = vec![("x-ig-set-www-claim".into(), "hmac.fresh".into())];
            Ok(match call.ask {
                Ask::Pk { .. } => Told::Pk {
                    answer,
                    pk: RouteAnswer::Pk(Pk::new(9001)),
                },
                _ => Told::Answer(answer),
            })
        });
        let (client, _) = spending_client(page.clone());
        let told = client
            .ask_page(
                Ask::Pk {
                    name: "someone".into(),
                },
                "/",
            )
            .await
            .unwrap();
        assert!(matches!(told, Told::Pk { pk: RouteAnswer::Pk(pk), .. } if pk == Pk::new(9001)));
        client.ask_page(query(), "/").await.unwrap();
        let claims: Vec<String> = page.asks().into_iter().map(|c| c.claim).collect();
        assert_eq!(claims, ["0", "hmac.fresh"]);
    }

    /// An opaque redirect is refused, with nothing more sent.
    #[tokio::test]
    async fn an_opaque_redirect_is_refused() {
        let page = Scripted::telling(|_| Ok(Told::Answer(page_said(0, ""))));
        let (client, budget) = spending_client(page);
        let error = client.ask_page(query(), "/").await.unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 0, .. }),
            "{error:?}"
        );
        assert_eq!(budget.reserved(), (1, 0));
    }

    /// Each ask, built by the tab's own step on an invented page: the
    /// request it sends, and what the tab answers itself.
    #[tokio::test]
    async fn the_tab_builds_what_is_asked_of_it() {
        let page = Scripted::building(|request| {
            Ok(
                if request.url.ends_with(crate::web::BULK_ROUTE_DEFINITIONS) {
                    page_said(
                        200,
                        r#"for (;;);{"payload":{"payloads":{"/someone/":{"error":false,
                        "result":{"exports":{"hostableView":{"props":{"id":"9001"}}}}}}}}"#,
                    )
                } else {
                    page_said(200, "<html>a document</html>")
                },
            )
        });
        let (client, budget) = spending_client(page.clone());

        let told = client.ask_page(Ask::Viewer, "/").await.unwrap();
        assert!(matches!(told, Told::Viewer(v) if v.pk == Pk::new(42)));
        let told = client
            .ask_page(
                Ask::Pk {
                    name: "someone".into(),
                },
                "/",
            )
            .await
            .unwrap();
        assert!(matches!(told, Told::Pk { pk: RouteAnswer::Pk(pk), .. } if pk == Pk::new(9001)));
        client.ask_page(query(), "/someone/").await.unwrap();
        let told = client
            .ask_page(
                Ask::Document {
                    path: "/someone/".into(),
                },
                "/",
            )
            .await
            .unwrap();
        let Told::Document { answer, bundles } = told else {
            panic!("a document is answered as one");
        };
        assert!(answer.body.is_empty() && bundles.is_empty());
        client.ask_page(follow(), "/someone/").await.unwrap();
        assert_eq!(budget.reserved(), (3, 1));

        let sent: Vec<(String, bool)> = page
            .asked()
            .into_iter()
            .map(|r| (r.url, r.navigate))
            .collect();
        assert_eq!(
            sent,
            [
                (
                    "http://127.0.0.1:9/ajax/bulk-route-definitions/".into(),
                    false
                ),
                ("http://127.0.0.1:9/api/graphql".into(), false),
                ("http://127.0.0.1:9/someone/".into(), true),
                ("http://127.0.0.1:9/api/graphql".into(), false),
            ]
        );
        let relay = &page.asked()[1];
        assert_eq!(
            relay.referrer, "http://127.0.0.1:9/someone/",
            "made from the page it names"
        );
        let body = relay.body.as_deref().unwrap();
        assert!(body.starts_with("av=17841400000000001&"), "{body}");
        assert!(
            body.contains("fb_api_req_friendly_name=PolarisUserHoverCardContentV2Query"),
            "{body}"
        );
    }

    /// Without a browser there is no page to ask.
    #[tokio::test]
    async fn a_client_without_a_page_asks_nothing() {
        let server = wiremock::MockServer::start().await;
        let client = crate::client::harness::client_with(&server, Pacer::unlimited());
        assert!(!client.has_page());
        let error = client.ask_page(Ask::Viewer, "/").await.unwrap_err();
        assert!(matches!(error, IgError::Browser(_)), "{error:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// A page that never answers.
    struct Stalled;

    impl Page for Stalled {
        fn send(&self, _: PageRequest) -> PageFuture<'_> {
            Box::pin(std::future::pending())
        }

        fn ask(&self, _: Call) -> AskFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    /// A page that answers a moment later.
    struct Slow;

    impl Page for Slow {
        fn send(&self, _: PageRequest) -> PageFuture<'_> {
            Box::pin(std::future::pending())
        }

        fn ask(&self, _: Call) -> AskFuture<'_> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(Told::Answer(page_said(200, "{}")))
            })
        }
    }
}
