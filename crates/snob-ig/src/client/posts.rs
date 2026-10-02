//! A profile's posts, one post, and its comments, read the way the recorded
//! web app reads them.
//!
//! - **The grid** is the profile's posts query, twelve posts a page, and its
//!   next page as the grid is scrolled: `PolarisProfilePostsQuery` and
//!   `PolarisProfilePostsTabContentQuery_connection`, asked by the account's
//!   name from its page. Each page is one read of the request budget. It is
//!   not charged to the day's accounts (`budget::accounts_per_day`): what it
//!   returns is posts, not a list of accounts.
//! - **A post** is `media/<pk>/info/`, the call the app sends the moment one
//!   opens, from a grid, a profile's reels or a message alike, made from the
//!   page it opens on (`/p/<code>/` or `/reel/<code>/`). The page itself is
//!   never loaded: a reel's link loaded cold lands on the app's reels
//!   screen, which reports what it plays.
//! - **Its comments** are `media/<pk>/comments/`, the call sent beside it,
//!   and the next pages by the `min_id` each page hands out. A page of
//!   comments is a list of the accounts that wrote them, so it is charged to
//!   the day's accounts as a list page is.
//!
//! **None of it tells anybody what was looked at.** The app reports the
//! posts and reels it shows through separate calls (view counts, the reels
//! screen's watched list), which snob has no code to send; the allowlist
//! refuses them and `crates/snob-core/tests/no_seen.rs` keeps them out of the
//! source.

use crate::allowlist::{Operation, Rest};
use crate::error::IgError;
use crate::model::post::{CommentsPage, Media, MediaInfo, TimelinePage};
use crate::shortcode::MediaPk;
use crate::web::{Ask, Told};

use super::IgClient;
use super::ask::{answered, profile_path, told_otherwise};

/// One page of a profile's grid: its posts, and where the next one starts.
#[derive(Debug, Clone)]
pub struct PostsPage {
    pub posts: Vec<Media>,
    /// `None` on the last page.
    pub next: Option<String>,
}

impl IgClient {
    /// A page of `username`'s posts: the first when `after` is `None`, and
    /// otherwise the one after the cursor the page before ended at. One read,
    /// made from the profile's page.
    ///
    /// **From the browser only.** The grid is a Relay query the tab builds
    /// from the page; without a browser there is no capture of a REST read of
    /// it to send instead, and none is invented.
    pub async fn profile_posts(
        &self,
        username: &str,
        after: Option<&str>,
    ) -> Result<PostsPage, IgError> {
        if !self.has_page() {
            return Err(IgError::Browser(
                "a profile's posts are read from the browser, and SNOB_NO_BROWSER has none".into(),
            ));
        }
        let (operation, variables) = match after {
            None => (
                Operation::ProfilePosts,
                crate::web::posts_variables(username),
            ),
            Some(after) => (
                Operation::ProfilePostsPage,
                crate::web::posts_page_variables(username, after),
            ),
        };
        let ask = Ask::Query {
            operation,
            variables,
        };
        let endpoint = ask.endpoint();
        let Told::Answer(answer) = self.ask_page(ask, &profile_path(username)).await? else {
            return Err(told_otherwise("a profile's posts"));
        };
        let page: TimelinePage =
            self.decode_query(&answered(&endpoint, answer), operation.root_field())?;
        let (posts, next) = page.posts();
        Ok(PostsPage { posts, next })
    }

    /// The post `pk`, asked from `page`, the page it opens on
    /// (`shortcode::page_of`). One read. `None` when the answer holds no
    /// post: one deleted, or not shown to this account.
    pub async fn media_info(&self, pk: MediaPk, page: &str) -> Result<Option<Media>, IgError> {
        let read = Rest::MediaInfo(pk);
        let info: MediaInfo = if self.has_page() {
            self.rest_read(read, &[], page).await?
        } else {
            self.get(&read.path(), &[], relative(page)).await?
        };
        Ok(info.items.into_iter().next())
    }

    /// A page of the comments of post `pk`, asked from `page`: the first when
    /// `after` is `None`, and otherwise the one the page before named in its
    /// `next_min_id`. One read, and its commenters charged to the day's
    /// accounts.
    pub async fn media_comments(
        &self,
        pk: MediaPk,
        after: Option<&str>,
        page: &str,
    ) -> Result<CommentsPage, IgError> {
        let read = Rest::Comments(pk);
        // In the app's order: the first page as the post opens, the next
        // ones as the comments scroll (capture of 2026-10-01).
        let query: Vec<(&str, &str)> = match after {
            None => vec![
                ("can_support_threading", "true"),
                ("permalink_enabled", "false"),
            ],
            Some(after) => vec![
                ("can_support_threading", "true"),
                ("min_id", after),
                ("sort_order", "popular"),
            ],
        };
        let comments: CommentsPage = if self.has_page() {
            self.rest_read(read, &query, page).await?
        } else {
            self.get(&read.path(), &query, relative(page)).await?
        };
        let accounts = u32::try_from(comments.accounts()).unwrap_or(u32::MAX);
        if let Err(e) = self.pacer.spend_accounts(accounts).await {
            tracing::warn!(error = %e, "could not record the accounts a page of comments carried");
        }
        Ok(comments)
    }
}

/// `page` as a request sent without a browser names its referrer: with no
/// leading slash, the site's address before it.
fn relative(page: &str) -> &str {
    page.strip_prefix('/').unwrap_or(page)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::client::harness::{NOWHERE, Scripted, page_said, spending_client};
    use crate::client::page::{Page, PageRequest};

    const GRID: &str = r#"{"data":{"xdt_api__v1__feed__user_timeline_graphql_connection":{
        "edges":[{"node":{"pk":"3807824420233075826","code":"DTYHCKvDNxy","media_type":1}}],
        "page_info":{"end_cursor":"AQH-next","has_next_page":true}}}}"#;

    /// A form field of a request the page was asked to send.
    fn field(request: &PageRequest, name: &str) -> Option<String> {
        url::form_urlencoded::parse(request.body.as_deref()?.as_bytes())
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.into_owned())
    }

    /// The first page is the posts query, the next one its connection from
    /// the cursor, both made from the profile's page and built as the tab
    /// builds them, each paid as one read.
    #[tokio::test]
    async fn the_grid_is_the_posts_query_and_then_its_connection() {
        let tab = Scripted::building(|_| Ok(page_said(200, GRID)));
        let (client, budget) = spending_client(Arc::clone(&tab) as Arc<dyn Page>);
        let first = client.profile_posts("someone", None).await.unwrap();
        assert_eq!(first.posts.len(), 1);
        assert_eq!(first.next.as_deref(), Some("AQH-next"));
        client
            .profile_posts("someone", first.next.as_deref())
            .await
            .unwrap();

        let sent = tab.asked();
        let names: Vec<_> = sent
            .iter()
            .filter_map(|r| field(r, "fb_api_req_friendly_name"))
            .collect();
        assert_eq!(
            names,
            [
                "PolarisProfilePostsQuery",
                "PolarisProfilePostsTabContentQuery_connection"
            ]
        );
        assert!(sent.iter().all(|r| r.url.ends_with("/graphql/query")));
        assert!(sent.iter().all(|r| r.referrer.ends_with("/someone/")));
        let next = field(&sent[1], "variables").unwrap();
        assert!(next.starts_with(r#"{"after":"AQH-next""#), "{next}");
        assert_eq!(budget.reserved(), (2, 0));
    }

    /// A post and its comments are the two GETs the app sends as it opens,
    /// made from the page it opens on; the next page of comments carries the
    /// cursor the first handed out, and the order.
    #[tokio::test]
    async fn a_post_is_its_info_and_its_comments_from_its_own_page() {
        let tab = Scripted::building(|request| {
            let body = if request.url.contains("/info/") {
                r#"{"items":[{"pk":"3947056156557494178","code":"DbGwql4IMei","media_type":2}],"status":"ok"}"#
            } else if request.url.contains("min_id=") {
                r#"{"comments":[],"has_more_headload_comments":false,"status":"ok"}"#
            } else {
                r#"{"comments":[{"pk":"1","text":"hi","user":{"username":"a"}}],"has_more_headload_comments":true,"next_min_id":"{\"c\":\"1\"}","status":"ok"}"#
            };
            Ok(page_said(200, body))
        });
        let (client, budget) = spending_client(Arc::clone(&tab) as Arc<dyn Page>);
        let pk = MediaPk::new(3_947_056_156_557_494_178);
        let page = "/reel/DbGwql4IMei/";
        let post = client.media_info(pk, page).await.unwrap();
        assert_eq!(post.unwrap().code.as_deref(), Some("DbGwql4IMei"));
        let first = client.media_comments(pk, None, page).await.unwrap();
        let next = first.next().map(str::to_string);
        assert_eq!(next.as_deref(), Some(r#"{"c":"1"}"#));
        let last = client
            .media_comments(pk, next.as_deref(), page)
            .await
            .unwrap();
        assert_eq!(last.next(), None);

        let sent = tab.asked();
        let urls: Vec<String> = sent
            .iter()
            .map(|r| {
                r.url
                    .trim_start_matches(NOWHERE.trim_end_matches('/'))
                    .to_string()
            })
            .collect();
        assert_eq!(
            urls,
            [
                "/api/v1/media/3947056156557494178/info/".to_string(),
                "/api/v1/media/3947056156557494178/comments/?can_support_threading=true&permalink_enabled=false".to_string(),
                format!(
                    "/api/v1/media/3947056156557494178/comments/?can_support_threading=true&min_id={}&sort_order=popular",
                    "%7B%22c%22%3A%221%22%7D"
                ),
            ]
        );
        assert!(sent.iter().all(|r| r.referrer.ends_with(page)));
        assert_eq!(budget.reserved(), (3, 0));
    }

    /// Without a browser the grid is not read, and nothing is spent finding
    /// that out.
    #[tokio::test]
    async fn without_a_browser_the_grid_is_refused_for_nothing() {
        let server = wiremock::MockServer::start().await;
        let client = crate::client::harness::client_with(&server, crate::pace::Pacer::unlimited());
        let refused = client.profile_posts("someone", None).await.unwrap_err();
        assert!(matches!(refused, IgError::Browser(_)), "{refused:?}");
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
    }
}
