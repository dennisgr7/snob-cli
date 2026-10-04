//! `snob fetch`: one file from Instagram's CDN, by an address already known.
//!
//! **No account is behind it.** The address came out of an authenticated
//! answer earlier (a listing's JSON, a post's items); fetching its bytes needs
//! nothing of the account, so this builds a [`CdnClient`] and nothing else: no
//! `App`, no session, no budget, no browser, no database. One of the listed
//! exceptions to "commands take an App", because an `App` is exactly the
//! account this does not use.
//!
//! The rule about where a file may come from is the client's own
//! ([`CdnClient::check_downloadable`]), the one every download is held to.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use snob_ig::client::CdnClient;
use snob_ig::error::IgError;

use crate::cli::FetchArgs;
use crate::exit::ExitCode;
use crate::output;
use crate::posts::MAX_POST_BYTES;
use crate::ui;

/// Fetches `args.url`, sending `user_agent` (the one given, or the
/// account's) or else the installed browser's.
pub async fn run(args: FetchArgs, user_agent: Option<String>) -> Result<ExitCode> {
    let user_agent = user_agent
        .filter(|ua| !ua.trim().is_empty())
        .or_else(|| crate::browser::detect().map(|browser| browser.user_agent()))
        .ok_or_else(|| {
            anyhow!(
                "there is no User-Agent to send: nobody is signed in and no browser was \
                 found. Give one with --user-agent"
            )
        })?;
    let cdn = CdnClient::new(
        &user_agent,
        snob_ig::client::site(),
        crate::interrupt::install(),
    )?;
    let url = url::Url::parse(&args.url).map_err(|e| anyhow!("that is not an address: {e}"))?;
    // Before anything else happens, so a refused address costs nothing and
    // creates no file.
    cdn.check_downloadable(&url)?;

    match args.output.as_deref() {
        Some(path) if output::is_stdout(path) => {
            output::one_file_to_stdout(1)?;
            to_stdout(&cdn, &args.url).await?;
        }
        Some(path) => {
            // The user named it, so replacing what is there is their call.
            let file = std::fs::File::create(path)
                .map_err(|e| anyhow!("could not write {}: {e}", path.display()))?;
            to_file(&cdn, &args.url, file, path).await?;
            ui::info(&format!("Saved {}", path.display()));
        }
        None if output::Presentation::detect(None).interactive => {
            // A name this program chose, out of an address: created, never
            // written over.
            let path = PathBuf::from(file_name_of(&url)?);
            let file = output::create_new(&path)?;
            to_file(&cdn, &args.url, file, &path).await?;
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

/// The file, streamed into `file` at `path`, which is removed again if the
/// download stops halfway: a truncated video under its real name reads as a
/// finished one.
async fn to_file(cdn: &CdnClient, url: &str, mut file: std::fs::File, path: &Path) -> Result<()> {
    if let Err(e) = cdn.download_to(url, MAX_POST_BYTES, &mut file).await {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(explained(e));
    }
    file.sync_all()
        .map_err(|e| anyhow!("could not finish writing {}: {e}", path.display()))
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

/// The address's own file name, kept to what is safe to create anywhere.
fn file_name_of(url: &url::Url) -> Result<String> {
    let last = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or_default();
    let (stem, extension) = last.rsplit_once('.').unwrap_or((last, "bin"));
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
    let extension = match keep(extension, 8) {
        extension if extension.is_empty() => "bin".to_string(),
        extension => extension.to_ascii_lowercase(),
    };
    let name = output::default_path(Path::new("."), &stem, &extension)?;
    Ok(name.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_name_is_the_addresses_own_and_nothing_else() {
        let name = |address: &str| file_name_of(&url::Url::parse(address).unwrap()).unwrap();
        assert_eq!(
            name("https://scontent.cdninstagram.com/v/t51/12345_n.jpg?oh=1&oe=2"),
            "12345_n.jpg"
        );
        assert_eq!(
            name("https://video.fbcdn.net/o1/v/AbC-dEf.MP4?efg=x"),
            "AbC-dEf.mp4"
        );
        assert_eq!(name("https://video.fbcdn.net/"), "download.bin");
        let sneaky = name("https://scontent.cdninstagram.com/..%2F..%2Fpasswd");
        assert!(
            !sneaky.contains('/') && !sneaky.contains('\\') && !sneaky.starts_with('.'),
            "{sneaky}"
        );
    }
}
