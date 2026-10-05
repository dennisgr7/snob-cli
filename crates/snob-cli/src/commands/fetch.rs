//! `snob fetch`: one file from Instagram's CDN, by an address already known.
//!
//! **No account is behind it.** The address came out of an authenticated
//! answer earlier (a listing's JSON, a post's items); fetching its bytes needs
//! nothing of the account, so this builds a [`CdnClient`] and nothing else: no
//! `App`, no budget, no browser, no database, and no session sent anywhere.
//! One of the listed exceptions to "commands take an App", because an `App` is
//! exactly the account this does not use. The one thing read off the account,
//! when `--user-agent` does not say it, is its browser's User-Agent.
//!
//! The rule about where a file may come from is the client's own
//! ([`CdnClient::check_downloadable`], `allowlist::refused_asset`), the one
//! every download is held to: the CDN's hosts, over HTTPS, and never
//! instagram.com itself.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use snob_ig::client::CdnClient;
use snob_ig::error::IgError;

use crate::cli::FetchArgs;
use crate::exit::ExitCode;
use crate::output;
use crate::posts::MAX_POST_BYTES;
use crate::ui;

/// Fetches `args.url`, sending `--user-agent`, else the one `account_ua`
/// reads off the account, else the installed browser's. Asked in that order
/// and only as far as needed, after the address has been judged: a refused
/// address costs nothing, not even a look for a browser.
pub async fn run(
    args: FetchArgs,
    account_ua: impl FnOnce() -> Result<Option<String>>,
) -> Result<ExitCode> {
    let url = url::Url::parse(&args.url).map_err(|e| anyhow!("that is not an address: {e}"))?;
    let site = snob_ig::client::site();
    if let Some(what) = snob_ig::allowlist::refused_asset(site.as_str(), url.as_str()) {
        return Err(anyhow!(
            "only files on Instagram's CDN (cdninstagram.com, fbcdn.net) are fetched, over \
             HTTPS; this is {what}"
        ));
    }
    let to_stdout_asked = args.output.as_deref().is_some_and(output::is_stdout);
    if to_stdout_asked {
        output::one_file_to_stdout(1)?;
    }

    let given = args.user_agent.clone().filter(|ua| !ua.trim().is_empty());
    let user_agent = match given {
        Some(given) => given,
        None => account_ua()?
            .filter(|ua| !ua.trim().is_empty())
            .or_else(|| crate::browser::detect().map(|browser| browser.user_agent()))
            .ok_or_else(|| {
                anyhow!(
                    "there is no User-Agent to send: nobody is signed in and no browser was \
                     found. Give one with --user-agent"
                )
            })?,
    };
    let cdn = CdnClient::new(&user_agent, site, crate::interrupt::install())?;
    // The client holds the address to the same rule again, and every
    // redirect hop after it.
    cdn.check_downloadable(&url)?;

    match args.output.as_deref() {
        Some(_) if to_stdout_asked => to_stdout(&cdn, &args.url).await?,
        Some(path) => {
            // The user named it, so replacing what is there is their call;
            // but only with a file that arrived whole.
            let mut file = output::Replacing::open(path)?;
            cdn.download_to(&args.url, MAX_POST_BYTES, file.file())
                .await
                .map_err(explained)?;
            file.commit()?;
            ui::info(&format!("Saved {}", path.display()));
        }
        None if output::Presentation::detect(None).interactive => {
            let path = to_named_file(&cdn, &url).await?;
            ui::info(&format!("Saved {}", path.display()));
        }
        None => to_stdout(&cdn, &args.url).await?,
    }
    Ok(ExitCode::Ok)
}

/// The file, streamed to standard output.
async fn to_stdout(cdn: &CdnClient, url: &str) -> Result<()> {
    let mut sink = output::StdoutSink::default();
    let streamed = cdn.download_to(url, MAX_POST_BYTES, &mut sink).await;
    sink.finish(streamed.map_err(explained))
}

/// The file, saved in the working directory under the address's own name
/// and the extension its bytes say, as every other download is named. It
/// streams to `<stem>.<pid>.part`, removed on any failure, and takes its
/// name only once it is whole; a name already there is never written over.
async fn to_named_file(cdn: &CdnClient, url: &url::Url) -> Result<PathBuf> {
    let (stem, from_address) = name_of(url);
    let dir = Path::new(".");
    let part = dir.join(format!("{stem}.{}.part", std::process::id()));
    let mut file = output::create_new(&part)?;
    let downloaded = match cdn
        .download_to(url.as_str(), MAX_POST_BYTES, &mut file)
        .await
    {
        Ok(downloaded) => downloaded,
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(explained(e));
        }
    };
    let synced = file.sync_all();
    drop(file);
    let placed = synced
        .map_err(|e| anyhow!("could not finish writing {}: {e}", part.display()))
        .and_then(|()| {
            let extension = extension_for(&downloaded.head, from_address.as_deref());
            output::default_path(dir, &stem, extension)
        })
        .and_then(|name| {
            std::fs::rename(&part, &name)
                .map_err(|e| anyhow!("could not move into place {}: {e}", name.display()))?;
            Ok(name)
        });
    if placed.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    placed
}

/// The CDN's refusal said in its own terms: a signed address that has
/// expired, rather than a picture that "could not be downloaded".
fn explained(error: IgError) -> anyhow::Error {
    match error {
        IgError::Unexpected {
            status: 403 | 410, ..
        } => anyhow!(
            "the CDN refused the address; a signed address expires after a while, so ask \
             for the post or the listing again for a fresh one"
        ),
        other => other.into(),
    }
}

/// The address's own file name, kept to what is safe to create anywhere: the
/// stem, and the extension it claims, which is only a fallback.
fn name_of(url: &url::Url) -> (String, Option<String>) {
    let last = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default();
    let (stem, extension) = match last.rsplit_once('.') {
        Some((stem, extension)) => (stem, Some(extension)),
        None => (last, None),
    };
    let keep = |text: &str, most: usize| -> String {
        text.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(most)
            .collect()
    };
    let stem = match keep(stem, 64) {
        stem if stem.is_empty() => "download".to_string(),
        stem => stem,
    };
    let extension = extension
        .map(|extension| keep(extension, 8).to_ascii_lowercase())
        .filter(|extension| !extension.is_empty());
    (stem, extension)
}

/// The extension a fetched file is saved under: what its bytes are, as
/// every other download is named, since the CDN converts (`stp=dst-jpg`) and
/// the address is no guide; failing that, the address's own, only when it is
/// one of the media the CDN serves; and otherwise `bin`, so a file that is
/// none of them is not saved under a name a system would run.
fn extension_for(head: &[u8], from_address: Option<&str>) -> &'static str {
    const MEDIA: [&str; 9] = [
        "jpg", "jpeg", "png", "webp", "heic", "gif", "mp4", "webm", "m4a",
    ];
    let jpeg = head.starts_with(&[0xFF, 0xD8, 0xFF]);
    let sniffed = crate::media::extension_of(head);
    if sniffed != "jpg" || jpeg {
        return sniffed;
    }
    from_address
        .and_then(|claimed| MEDIA.iter().find(|media| **media == claimed))
        .copied()
        .unwrap_or("bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_name_is_the_addresses_own_and_nothing_else() {
        let name = |address: &str| name_of(&url::Url::parse(address).unwrap());
        assert_eq!(
            name("https://scontent.cdninstagram.com/v/t51/12345_n.jpg?oh=1&oe=2"),
            ("12345_n".to_string(), Some("jpg".to_string()))
        );
        assert_eq!(
            name("https://video.fbcdn.net/o1/v/AbC-dEf.MP4?efg=x"),
            ("AbC-dEf".to_string(), Some("mp4".to_string()))
        );
        assert_eq!(
            name("https://video.fbcdn.net/"),
            ("download".to_string(), None)
        );
        let (sneaky, _) = name("https://scontent.cdninstagram.com/..%2F..%2Fpasswd");
        assert!(
            !sneaky.contains('/') && !sneaky.contains('\\') && !sneaky.starts_with('.'),
            "{sneaky}"
        );
    }

    /// The bytes decide, the address only vouches for media, and anything
    /// else is `bin`, whatever the address says it is.
    #[test]
    fn the_extension_is_read_from_the_bytes_first() {
        let mp4 = b"\x00\x00\x00\x18ftypisom";
        assert_eq!(extension_for(mp4, Some("jpg")), "mp4");
        assert_eq!(
            extension_for(b"\xff\xd8\xff\xe0 a picture", Some("webp")),
            "jpg"
        );
        assert_eq!(extension_for(b"\x89PNG\r\n", None), "png");
        assert_eq!(extension_for(b"\x1aE\xdf\xa3 webm", Some("webm")), "webm");
        assert_eq!(extension_for(b"MZ\x90\x00", Some("exe")), "bin");
        assert_eq!(extension_for(b"function(){}", Some("js")), "bin");
        assert_eq!(extension_for(b"#!/bin/sh", None), "bin");
    }
}
