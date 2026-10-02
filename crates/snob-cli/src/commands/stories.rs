//! `snob stories`: what an account has up right now, and how to keep a copy.
//!
//! The requests for the listing, from the browser: the profile (two
//! requests the first time a name is seen, its id and then the profile) and
//! the reel, which is the web client's story gallery, opened over the
//! stories tray the home page shows, read off the page for nothing. Without
//! one: the profile, by name, and the reel. Then one more per story
//! downloaded, from the CDN, which is a different host with its own limits
//! and is not charged against Instagram's budget. The same shape as `pfp`,
//! for the same reasons, and like `pfp` it walks no list and stores no
//! snapshot.
//!
//! **Downloading a story does not mark it as seen.** Registering a view is a
//! separate call, and a write — so it falls under the two-write rule and this
//! project has no code that could send it; `crates/snob-core/tests/no_seen.rs`
//! reads the source of every crate to keep it that way. The consequence
//! belongs to the person whose story it is, whose viewer list stays a record of
//! people who opened it in the app, and it is the one property here that nobody
//! running the tool would ever notice being broken.
//!
//! **It does not ask for consent, and that is deliberate.** The consent rule
//! covers enumerating somebody — walking their followers, which is thousands of
//! requests about thousands of people who did not ask to be in a database.
//! Reading one reel is one request about one account, and `pfp` does not ask
//! either. Asking here would make the question routine, and a question that is
//! always asked stops being read.

use std::path::Path;

use anyhow::{Result, anyhow};
use comfy_table::Cell;
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_ig::model::{ReelItem, largest};
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::app::{App, Viewer};
use crate::cli::{Format, StoriesArgs, StoryFormat};
use crate::commands::common;
use crate::exit::ExitCode;
use crate::media::{self, Kind, Stories, Story, empty_document, left_of, posted_of};
use crate::output::Presentation;
use crate::ui;

pub async fn run(args: StoriesArgs, store: SecretStore, paths: &AccountPaths) -> Result<ExitCode> {
    let app = common::reader(&store, paths, args.action.interactive, false)?;
    let typed = common::target_or_own(&app, args.target.as_deref()).await?;

    let known = crate::engine::target::known_pk_of(&app, args.target.as_deref(), &typed)?;
    let stories = fetch(app.client(), &typed, known).await?;

    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }

    if stories.items.is_empty() {
        ui::info(&format!(
            "@{} has no stories up right now.",
            printable(&stories.username)
        ));
        if args.action.selection().is_none() && !args.action.interactive {
            let destination = args.action.output.as_deref();
            let format = common::checked_format(args.list.format, destination, "a story listing")?;
            empty_document(format, destination, || as_json(&stories, format))?;
        }
        return Ok(ExitCode::Ok);
    }

    // The browser is the default for a person at a terminal; every explicit
    // flag beats detection. `MediaActionArgs::browses` is the whole rule.
    if args.action.browses(
        args.list.format.is_some(),
        ui::a_human_would_watch_the_listing_scroll_by(),
    ) {
        return browse(app, stories, &store, paths).await;
    }

    if let Some(selection) = args.action.selection() {
        return media::download_selected(
            app.client_shared(),
            printable(&stories.username),
            &stories.items,
            selection,
            args.action.output.as_deref(),
            |number| no_such_story(&stories, number),
        )
        .await;
    }

    list(&stories, args.list.format, args.action.output.as_deref())
}

/// The list, as the account the command runs as and then as each account the
/// list switches to, fetched again as that one: an account's stories can be
/// hidden from one viewer and not another.
async fn browse(
    app: Box<App>,
    stories: Stories,
    store: &SecretStore,
    paths: &AccountPaths,
) -> Result<ExitCode> {
    common::switching(
        (app, stories),
        async |(app, stories): &mut (Box<App>, Stories), note: String| {
            crate::ui::stories::browse(app.client(), stories, app.viewer(), paths, store, note)
                .await
        },
        async |(app, stories): &(Box<App>, Stories), to: Viewer| {
            let name = stories.username.clone();
            common::switch(
                store,
                paths,
                app.viewer(),
                &to,
                false,
                async |app: &mut App| {
                    let known = crate::engine::target::known_pk(app, &name)?;
                    let stories = fetch(app.client(), &name, known).await?;
                    if stories.items.is_empty() {
                        anyhow::bail!("@{} has no stories up for it", printable(&stories.username));
                    }
                    Ok(stories)
                },
            )
            .await
        },
        |_: &(Box<App>, Stories), _: bool| {},
    )
    .await
}

/// The sentence for a number the tray does not have.
fn no_such_story(stories: &Stories, number: usize) -> anyhow::Error {
    anyhow!(
        "there is no story {number}: @{} has {}",
        printable(&stories.username),
        match stories.items.len() {
            1 => "one".to_string(),
            n => format!("{n} stories"),
        }
    )
}

/// The network half: the profile and the reel, each paid for inside the
/// client, for the reason `pfp`'s fetch gives, kept apart from the session
/// and the filesystem so a test can drive it.
/// `known` is the account's pk when this machine has seen it.
pub async fn fetch(
    client: &IgClient,
    typed: &str,
    known: Option<snob_core::Pk>,
) -> Result<Stories> {
    let profile = client
        .profile_named(crate::engine::target::clean(typed), known)
        .await?;

    let Some(reel) = client.stories(profile.id, &profile.username).await? else {
        return Ok(Stories {
            username: profile.username,
            items: Vec::new(),
        });
    };

    Ok(Stories {
        username: profile.username,
        items: reel.items.iter().map(story_from).collect(),
    })
}

/// The wire item, reduced.
///
/// The largest version wins rather than the first. Instagram lists candidates
/// in an order it does not promise, and the web client picks the one that fits
/// its viewport — nothing here draws in a terminal, so what is wanted is simply
/// the biggest. `image_versions2` is read even for a video, because a video
/// item carries its poster frame there and an item with no `video_versions` is
/// then still downloadable as the picture Instagram has of it.
pub(crate) fn story_from(item: &ReelItem) -> Story {
    let kind = Kind::of(item.media_type);
    let video = largest(&item.video_versions).map(|v| v.url.clone());
    let image = item
        .image_versions2
        .as_ref()
        .and_then(|c| largest(&c.candidates))
        .map(|v| v.url.clone());

    Story {
        kind,
        taken_at: item.taken_at,
        expiring_at: item.expiring_at,
        url: match kind {
            Kind::Video => video.or(image),
            _ => image.or(video),
        },
        mentions: item.mentioned().map(printable).collect(),
    }
}

/// Prints the listing. The numbers here are what `--download` takes.
fn list(
    stories: &Stories,
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = common::checked_format(format, destination, "a story listing")?;
    let text = match format {
        Format::Json | Format::Ndjson => as_json(stories, format)?,
        _ => as_table(stories, Presentation::detect(destination)),
    };
    media::print_listing(
        text,
        destination,
        "--download <number> to download one, or --interactive to move through them",
    )
}

fn as_table(stories: &Stories, presentation: Presentation) -> String {
    let mut table = media::listing_table(
        &["#", "Kind", "Posted", "Gone in", "Mentions"],
        presentation,
    );

    for (index, story) in stories.items.iter().enumerate() {
        table.add_row([
            Cell::new(index + 1),
            Cell::new(story.kind.label()),
            Cell::new(posted_of(story)),
            Cell::new(left_of(story)),
            Cell::new(media::mentions_of(story)),
        ]);
    }
    table.to_string()
}

fn as_json(stories: &Stories, format: Format) -> Result<String> {
    let rows: Vec<serde_json::Value> = stories
        .items
        .iter()
        .enumerate()
        .map(|(index, story)| {
            serde_json::json!({
                "number": index + 1,
                "kind": story.kind.label(),
                "taken_at": story.taken_at,
                "expiring_at": story.expiring_at,
                "mentions": story.mentions,
                // Deliberately included: a signed CDN address is what makes the
                // JSON usable by anything else, and it is already in the reply
                // Instagram gave this session. It expires on its own.
                "url": story.url,
            })
        })
        .collect();

    media::rows_or_envelope(rows, format, |rows| {
        serde_json::json!({
            "username": stories.username,
            "stories": rows,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report;
    use snob_core::Epoch;
    use snob_ig::model::{Candidates, PictureVersion};

    fn version(url: &str, w: u32, h: u32) -> PictureVersion {
        PictureVersion {
            url: url.into(),
            width: Some(w),
            height: Some(h),
        }
    }

    fn item(media_type: u8, images: Vec<PictureVersion>, videos: Vec<PictureVersion>) -> ReelItem {
        ReelItem {
            pk: "1".into(),
            media_type,
            taken_at: Epoch::new(1_000),
            expiring_at: Some(Epoch::new(2_000)),
            image_versions2: Some(Candidates { candidates: images }),
            video_versions: videos,
            reel_mentions: Vec::new(),
            story_bloks_stickers: Vec::new(),
        }
    }

    #[test]
    fn the_biggest_version_wins_not_the_first() {
        let story = story_from(&item(
            1,
            vec![version("small", 320, 320), version("big", 1080, 1920)],
            vec![],
        ));
        assert_eq!(story.url.as_deref(), Some("big"));
    }

    /// A video item carries a poster frame in `image_versions2`. Taking the
    /// first URL of either list would hand back the poster for a video, which
    /// is a picture where a video was asked for.
    #[test]
    fn a_video_prefers_its_video_over_its_poster_frame() {
        let story = story_from(&item(
            2,
            vec![version("poster", 1080, 1920)],
            vec![version("clip", 720, 1280)],
        ));
        assert_eq!(story.kind, Kind::Video);
        assert_eq!(story.url.as_deref(), Some("clip"));
    }

    /// And falls back to it rather than to nothing, because a poster is more
    /// use than a refusal.
    #[test]
    fn a_video_with_no_video_falls_back_to_the_poster() {
        let story = story_from(&item(2, vec![version("poster", 1080, 1920)], vec![]));
        assert_eq!(story.url.as_deref(), Some("poster"));
    }

    /// A media type nobody has seen before is downloadable and honestly
    /// labeled, rather than called a photo and given a `.jpg`.
    #[test]
    fn an_unknown_media_type_is_not_guessed_at() {
        let story = story_from(&item(9, vec![version("something", 100, 100)], vec![]));
        assert_eq!(story.kind, Kind::Unknown);
        assert_eq!(story.kind.label(), "unknown");
        assert_eq!(story.url.as_deref(), Some("something"));
    }

    fn plain() -> Presentation {
        Presentation {
            interactive: false,
            hyperlinks: false,
            color: false,
            width: Some(120),
        }
    }

    /// A photo tagging two accounts and a video tagging nobody, posted at
    /// midday UTC on the fifteenth, so the drawn date has one width in every
    /// zone.
    fn two_stories() -> Stories {
        let story = |kind, mentions: &[&str]| Story {
            kind,
            taken_at: Epoch::new(1_773_576_000),
            expiring_at: None,
            url: Some(format!("https://cdn.example/{}", mentions.len())),
            mentions: mentions.iter().map(ToString::to_string).collect(),
        };
        Stories {
            username: "someone".into(),
            items: vec![story(Kind::Photo, &["a", "b"]), story(Kind::Video, &[])],
        }
    }

    #[test]
    fn the_table_draws_what_the_listing_numbers() {
        let posted = report::stored_on(Epoch::new(1_773_576_000));
        assert_eq!(
            as_table(&two_stories(), plain()),
            [
                "┌───┬───────┬─────────────────┬─────────┬──────────┐".to_string(),
                "│ # ┆ Kind  ┆ Posted          ┆ Gone in ┆ Mentions │".to_string(),
                "╞═══╪═══════╪═════════════════╪═════════╪══════════╡".to_string(),
                format!("│ 1 ┆ photo ┆ {posted} ┆ -       ┆ @a @b    │"),
                format!("│ 2 ┆ video ┆ {posted} ┆ -       ┆          │"),
                "└───┴───────┴─────────────────┴─────────┴──────────┘".to_string(),
            ]
            .join("\n")
        );
    }

    /// NDJSON is the rows alone, one to a line; JSON wraps them with the
    /// account they belong to.
    #[test]
    fn the_json_is_the_rows_or_the_rows_inside_their_account() {
        let stories = two_stories();
        let rows = [
            serde_json::json!({"number": 1, "kind": "photo", "taken_at": 1_773_576_000,
                "expiring_at": null, "mentions": ["a", "b"], "url": "https://cdn.example/2"}),
            serde_json::json!({"number": 2, "kind": "video", "taken_at": 1_773_576_000,
                "expiring_at": null, "mentions": [], "url": "https://cdn.example/0"}),
        ];
        let lines: Vec<serde_json::Value> = as_json(&stories, Format::Ndjson)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines, rows);
        let document: serde_json::Value =
            serde_json::from_str(&as_json(&stories, Format::Json).unwrap()).unwrap();
        assert_eq!(
            document,
            serde_json::json!({"username": "someone", "stories": rows})
        );
    }
}
