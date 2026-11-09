//! Asking GitHub for the latest release, and downloading its files.
//!
//! **GitHub and nothing else, carrying nothing.** The client is
//! `snob_ig::http::plain_download`, which has no argument for a credential,
//! says `User-Agent: snob/<version>` (GitHub refuses a request without
//! one), and follows a redirect only to the hosts a release is served from,
//! over HTTPS ([`Origin::allows`]), as the CDN client holds every hop to its
//! own rule. One question a day at most, sent with the answer's `ETag` so an
//! unchanged one costs nothing; no retry, since tomorrow's check is the
//! retry.
//!
//! **Every file is checked against `SHA256SUMS`** as it arrives, the same
//! check `install.sh` and `install.ps1` make, and refused on any difference
//! or past its ceiling ([`MOST_BYTES`]).

use std::io::Write;
use std::path::Path;

use sha2::{Digest, Sha256};
use snob_ig::http::reqwest;
use url::Url;

use super::REPO;
use super::version::Version;

/// The most a release archive may be. Every one so far is under 15 MB; the
/// ceiling is what bounds a download that has no deadline on the whole.
pub(crate) const MOST_BYTES: u64 = 128 * 1024 * 1024;

/// The most an answer of the API or `SHA256SUMS` may be.
const MOST_ANSWER: usize = 1024 * 1024;

/// How many redirects a download may follow: one is what GitHub sends, from
/// `github.com` to its storage.
const MOST_HOPS: usize = 3;

/// Where releases are asked about and downloaded from.
#[derive(Debug, Clone)]
pub(crate) struct Origin {
    /// The API, for the latest release.
    pub(crate) api: Url,
    /// Where a release's files are, by tag and name.
    pub(crate) downloads: Url,
    /// The hosts a request or a redirect may reach.
    hosts: Vec<String>,
}

impl Origin {
    /// GitHub's.
    pub(crate) fn github() -> Self {
        Self {
            api: Url::parse("https://api.github.com/").expect("a valid address"),
            downloads: Url::parse("https://github.com/").expect("a valid address"),
            hosts: [
                "api.github.com",
                "github.com",
                "objects.githubusercontent.com",
                "release-assets.githubusercontent.com",
            ]
            .map(String::from)
            .to_vec(),
        }
    }

    /// A fake GitHub at `base`, for tests: the one host, over plain HTTP.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn at(base: &Url) -> Self {
        Self {
            api: base.clone(),
            downloads: base.clone(),
            hosts: base.host_str().map(String::from).into_iter().collect(),
        }
    }

    /// Whether `url` may be reached: one of the hosts, over HTTPS (or over
    /// the plain HTTP of a fake GitHub on this machine, for tests).
    fn allows(&self, url: &Url) -> bool {
        let secure = url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")));
        secure
            && url
                .host_str()
                .is_some_and(|host| self.hosts.iter().any(|h| h == host))
    }

    fn client(&self) -> anyhow::Result<reqwest::Client> {
        let origin = self.clone();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MOST_HOPS {
                attempt.error("too many redirects")
            } else if origin.allows(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("a redirect tried to leave GitHub")
            }
        });
        let agent = format!("snob/{}", env!("CARGO_PKG_VERSION"));
        Ok(snob_ig::http::plain_download(&agent, policy)?)
    }

    fn checked(&self, url: Url) -> anyhow::Result<Url> {
        anyhow::ensure!(self.allows(&url), "{url} is not GitHub's");
        Ok(url)
    }
}

/// What GitHub said of the latest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Latest {
    /// The same answer as last time (`304 Not Modified`).
    Unchanged,
    /// A release, and the tag of the answer.
    Release {
        version: Version,
        etag: Option<String>,
    },
}

/// Asks for the latest release, with the tag of the last answer.
pub(crate) async fn latest(origin: &Origin, etag: Option<&str>) -> anyhow::Result<Latest> {
    let url = origin.checked(origin.api.join(&format!("repos/{REPO}/releases/latest"))?)?;
    let mut request = origin
        .client()?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    let response = request.send().await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Latest::Unchanged);
    }
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {} about the latest release",
        response.status()
    );
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    let body = snob_ig::http::read_capped(response, MOST_ANSWER).await?;
    let release: serde_json::Value = serde_json::from_str(&body)?;
    // `/latest` names neither a draft nor a pre-release; asked anyway, so a
    // change on GitHub's side cannot hand one over.
    let flagged = |key: &str| release[key].as_bool().unwrap_or(false);
    anyhow::ensure!(
        !flagged("draft") && !flagged("prerelease"),
        "GitHub named a draft or a pre-release as the latest release"
    );
    let tag = release["tag_name"].as_str().unwrap_or_default();
    let version = Version::parse(tag)
        .ok_or_else(|| anyhow::anyhow!("the latest release's tag {tag:?} is no version"))?;
    Ok(Latest::Release { version, etag })
}

/// The address of `name` in the release of `version`.
fn file_url(origin: &Origin, version: Version, name: &str) -> anyhow::Result<Url> {
    origin.checked(
        origin
            .downloads
            .join(&format!("{REPO}/releases/download/v{version}/{name}"))?,
    )
}

/// `SHA256SUMS` of the release of `version`.
pub(crate) async fn sums(origin: &Origin, version: Version) -> anyhow::Result<String> {
    let response = origin
        .client()?
        .get(file_url(origin, version, "SHA256SUMS")?)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {} for the release's SHA256SUMS",
        response.status()
    );
    Ok(snob_ig::http::read_capped(response, MOST_ANSWER).await?)
}

/// The checksum `sums` lists for `name`, in lowercase hex. The format is
/// `sha256sum`'s: the hash, a space, a space or `*`, the name.
pub(crate) fn listed(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.split_once(' ')?;
        let file = file.strip_prefix(['*', ' ']).unwrap_or(file).trim_end();
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

/// Downloads `name` of the release of `version` to `to`, checking it against
/// `expected` (lowercase hex) as it arrives. Written to `to` plus `.part`
/// and renamed only once it is whole and right, so a file at `to` is always
/// one that was checked.
pub(crate) async fn download(
    origin: &Origin,
    version: Version,
    name: &str,
    expected: &str,
    to: &Path,
) -> anyhow::Result<()> {
    let mut response = origin
        .client()?
        .get(file_url(origin, version, name)?)
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {} for {name}",
        response.status()
    );
    if let Some(declared) = response.content_length() {
        anyhow::ensure!(
            declared <= MOST_BYTES,
            "{name} is {declared} bytes, more than allowed"
        );
    }
    let part = to.with_extension("part");
    let mut file = std::fs::File::create(&part)?;
    let mut hash = Sha256::new();
    let mut read: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        read += chunk.len() as u64;
        anyhow::ensure!(read <= MOST_BYTES, "{name} is more than {MOST_BYTES} bytes");
        hash.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    drop(file);
    let got = hex(&hash.finalize());
    if got != expected {
        let _ = std::fs::remove_file(&part);
        anyhow::bail!("{name} does not match SHA256SUMS: expected {expected}, got {got}");
    }
    std::fs::rename(&part, to)?;
    Ok(())
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 of the file at `path`, in lowercase hex.
pub(crate) fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(hex(&hash.finalize()))
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const RELEASE: &str = "/repos/dennisgr7/snob-cli/releases/latest";

    #[test]
    fn sums_are_read_as_sha256sum_writes_them() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!("{a}  snob-v1.0.0-x.tar.gz\n{b} *snob-v1.0.0-y.zip\nnonsense\n");
        assert_eq!(listed(&sums, "snob-v1.0.0-x.tar.gz"), Some(a));
        assert_eq!(listed(&sums, "snob-v1.0.0-y.zip"), Some("b".repeat(64)));
        assert_eq!(listed(&sums, "snob-v1.0.0-z.zip"), None);
        assert_eq!(listed("abc  short\n", "short"), None);
    }

    #[test]
    fn only_githubs_hosts_over_https_are_reached() {
        let github = Origin::github();
        for allowed in [
            "https://api.github.com/x",
            "https://github.com/dennisgr7/snob-cli/releases/download/v1.0.0/SHA256SUMS",
            "https://objects.githubusercontent.com/x",
            "https://release-assets.githubusercontent.com/x",
        ] {
            assert!(github.allows(&Url::parse(allowed).unwrap()), "{allowed}");
        }
        for refused in [
            "http://github.com/x",
            "https://github.com.example.com/x",
            "https://example.com/x",
            "http://127.0.0.1/x",
        ] {
            assert!(!github.allows(&Url::parse(refused).unwrap()), "{refused}");
        }
    }

    #[tokio::test]
    async fn the_latest_release_is_read_and_an_unchanged_one_costs_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(RELEASE))
            .and(header("if-none-match", "\"seen\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(RELEASE))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"seen\"")
                    .set_body_string(r#"{"tag_name":"v0.10.0","draft":false,"prerelease":false}"#),
            )
            .mount(&server)
            .await;
        let origin = Origin::at(&Url::parse(&server.uri()).unwrap());

        let first = latest(&origin, None).await.unwrap();
        assert_eq!(
            first,
            Latest::Release {
                version: Version::parse("0.10.0").unwrap(),
                etag: Some("\"seen\"".into())
            }
        );
        assert_eq!(
            latest(&origin, Some("\"seen\"")).await.unwrap(),
            Latest::Unchanged
        );
        let asked = &server.received_requests().await.unwrap()[0];
        assert!(asked.headers.get("cookie").is_none());
        assert!(
            asked.headers["user-agent"]
                .to_str()
                .unwrap()
                .starts_with("snob/")
        );
    }

    #[tokio::test]
    async fn a_pre_release_is_never_the_latest() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(RELEASE))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"tag_name":"v0.10.0","prerelease":true}"#),
            )
            .mount(&server)
            .await;
        let origin = Origin::at(&Url::parse(&server.uri()).unwrap());
        assert!(latest(&origin, None).await.is_err());
    }

    #[tokio::test]
    async fn a_download_that_does_not_match_its_sum_leaves_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/dennisgr7/snob-cli/releases/download/v0.10.0/a.tar.gz",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("the archive"))
            .mount(&server)
            .await;
        let origin = Origin::at(&Url::parse(&server.uri()).unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let to = tmp.path().join("a.tar.gz");
        let version = Version::parse("0.10.0").unwrap();

        let wrong = "0".repeat(64);
        assert!(
            download(&origin, version, "a.tar.gz", &wrong, &to)
                .await
                .is_err()
        );
        assert!(!to.exists());
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());

        let right = hex(&Sha256::digest(b"the archive"));
        download(&origin, version, "a.tar.gz", &right, &to)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"the archive");
        assert_eq!(sha256_of(&to).unwrap(), right);
    }

    /// A redirect is followed only to a host a release is served from.
    #[tokio::test]
    async fn a_redirect_off_github_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/dennisgr7/snob-cli/releases/download/v0.10.0/SHA256SUMS",
            ))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://example.com/sums"),
            )
            .mount(&server)
            .await;
        let origin = Origin::at(&Url::parse(&server.uri()).unwrap());
        assert!(
            sums(&origin, Version::parse("0.10.0").unwrap())
                .await
                .is_err()
        );
    }
}
