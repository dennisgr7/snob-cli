//! A video's DASH manifest (`video_dash_manifest`), read for the one thing
//! snob wants from it: where the widest video track and the sound track are.
//!
//! The progressive file a post offers (`video_versions`) is 720 wide on 160
//! of 180 post videos of the capture of 2026-11-13; the manifest beside it
//! holds a wider rendition on 165 of them, usually 1080, as a video track and
//! a separate sound track, and that is what the app's player plays.
//!
//! The manifest is text Instagram wrote, read in a process that aborts on a
//! panic (`panic = "abort"`), so it is read by hand with `find` and `get`
//! only: no indexing, no XML crate, bounded length. **A manifest this cannot
//! read is no manifest**: the caller falls back to the progressive file, and
//! nothing fails because of it.
//!
//! What it holds, in that capture (195 manifests): one `Period`, one video
//! `AdaptationSet` of 1 to 9 `Representation`s and, on 191, an audio one with
//! a single `Representation`; each `Representation` has its `width`,
//! `height`, `bandwidth`, `codecs`, `mimeType` (`video/mp4` or `audio/mp4`),
//! `FBContentLength` (bytes) and one `BaseURL`, a whole file, whose `&` are
//! written `&amp;` (on 194 of 195). No `SegmentTemplate`, no CDATA.

/// The longest manifest read. The longest seen was about 19 KB (2026-11-13).
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// The most renditions read from one manifest. Nine was the most seen
/// (2026-11-13); a manifest with more is not one this was written for.
pub const MOST_REPRESENTATIONS: usize = 32;

/// One track of a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// The whole file, its entities decoded.
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Bits a second.
    pub bandwidth: Option<u64>,
    /// `vp09...`, `avc1...`, `mp4a...`.
    pub codecs: Option<String>,
    /// Its size, as the manifest states it, before any byte is fetched.
    pub bytes: Option<u64>,
}

/// The widest video track of a manifest and its sound, when it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracks {
    pub video: Track,
    pub audio: Option<Track>,
}

/// The widest video track of `manifest`, the highest `bandwidth` among
/// equals, and the first sound track; `None` when it holds no video track
/// this can read, or is not one this was written for.
pub fn tracks(manifest: &str) -> Option<Tracks> {
    if manifest.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let mut videos: Vec<Track> = Vec::new();
    let mut audios: Vec<Track> = Vec::new();
    let mut rest = manifest;
    let mut seen = 0;
    while let Some(start) = rest.find("<Representation ") {
        seen += 1;
        if seen > MOST_REPRESENTATIONS {
            return None;
        }
        let from = rest.get(start..)?;
        let open = from.get(..from.find('>')?)?;
        let end = from.find("</Representation>")?;
        let body = from.get(..end)?;
        rest = from.get(end + "</Representation>".len()..)?;

        // Where it may be fetched from is not judged here: every download
        // is held to the CDN's hosts over HTTPS (`cdn::check_downloadable`)
        // before anything is asked.
        let Some(url) = between(body, "<BaseURL>", "</BaseURL>").map(decoded) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let track = Track {
            url,
            width: attribute(open, "width").and_then(|w| w.parse().ok()),
            height: attribute(open, "height").and_then(|h| h.parse().ok()),
            bandwidth: attribute(open, "bandwidth").and_then(|b| b.parse().ok()),
            codecs: attribute(open, "codecs").map(decoded),
            bytes: attribute(open, "FBContentLength").and_then(|b| b.parse().ok()),
        };
        match attribute(open, "mimeType") {
            Some(mime) if mime.starts_with("audio/") => audios.push(track),
            Some(mime) if mime.starts_with("video/") => videos.push(track),
            // With no type said, a track with a width is a picture's.
            None if track.width.is_some() => videos.push(track),
            _ => {}
        }
    }
    let video = videos
        .into_iter()
        .rev()
        .max_by_key(|t| (t.width.unwrap_or(0), t.bandwidth.unwrap_or(0)))?;
    Some(Tracks {
        video,
        audio: audios.into_iter().next(),
    })
}

/// The value of attribute `name` in the opening tag `open`, as written.
fn attribute<'a>(open: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let from = open.get(open.find(&key)? + key.len()..)?;
    from.get(..from.find('"')?)
}

/// What sits between `first` and the next `last` in `text`.
fn between<'a>(text: &'a str, first: &str, last: &str) -> Option<&'a str> {
    let from = text.get(text.find(first)? + first.len()..)?;
    from.get(..from.find(last)?)
}

/// `text` with XML's five named entities decoded, `&amp;` last so that an
/// escaped entity comes out as written.
fn decoded(text: &str) -> String {
    text.trim()
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representation as the capture shapes it, with `&amp;` in its
    /// address.
    fn representation(mime: &str, width: u32, bandwidth: u64, name: &str) -> String {
        format!(
            concat!(
                r#"<Representation id="{name}" bandwidth="{bandwidth}" codecs="vp09.00.31.08" "#,
                r#"mimeType="{mime}" width="{width}" height="{height}" FBContentLength="{bytes}" "#,
                r#"FBQualityLabel="x"><BaseURL>https://x.fbcdn.net/{name}.mp4?a=1&amp;b=2</BaseURL>"#,
                r#"<SegmentBase indexRange="818-933"><Initialization range="0-817"/></SegmentBase>"#,
                r#"</Representation>"#
            ),
            name = name,
            bandwidth = bandwidth,
            mime = mime,
            width = width,
            height = width * 16 / 9,
            bytes = bandwidth * 10,
        )
    }

    fn manifest(representations: &[String]) -> String {
        format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?><MPD type="static"><Period id="0">"#,
                r#"<AdaptationSet id="0" contentType="video">{}</AdaptationSet></Period></MPD>"#
            ),
            representations.concat()
        )
    }

    #[test]
    fn the_widest_representation_wins_and_bandwidth_breaks_a_tie() {
        let text = manifest(&[
            representation("video/mp4", 720, 900, "v720"),
            representation("video/mp4", 1080, 2_000, "v1080low"),
            representation("video/mp4", 1080, 3_000, "v1080"),
            representation("video/mp4", 576, 100, "v576"),
            representation("audio/mp4", 0, 64, "a"),
        ]);
        let tracks = tracks(&text).unwrap();
        assert_eq!(tracks.video.url, "https://x.fbcdn.net/v1080.mp4?a=1&b=2");
        assert_eq!(tracks.video.width, Some(1080));
        assert_eq!(tracks.video.height, Some(1920));
        assert_eq!(tracks.video.bandwidth, Some(3_000));
        assert_eq!(tracks.video.bytes, Some(30_000));
        assert_eq!(tracks.video.codecs.as_deref(), Some("vp09.00.31.08"));
        assert_eq!(
            tracks.audio.map(|a| a.url),
            Some("https://x.fbcdn.net/a.mp4?a=1&b=2".to_string())
        );
    }

    #[test]
    fn entities_in_an_address_are_decoded() {
        assert_eq!(decoded(" a&amp;b&lt;c&gt;d&quot;e&apos;f "), "a&b<c>d\"e'f");
        assert_eq!(decoded("&amp;lt;"), "&lt;");
    }

    #[test]
    fn a_manifest_with_no_audio_gives_the_video_alone() {
        let text = manifest(&[representation("video/mp4", 1080, 3_000, "v")]);
        let tracks = tracks(&text).unwrap();
        assert_eq!(tracks.video.width, Some(1080));
        assert_eq!(tracks.audio, None);
    }

    #[test]
    fn a_manifest_cut_short_reads_as_none() {
        let whole = manifest(&[
            representation("video/mp4", 1080, 3_000, "v"),
            representation("audio/mp4", 0, 64, "a"),
        ]);
        // Every cut of it, at every character boundary, reads or does not,
        // and never panics; one cut before the video's end is no video.
        for (cut, _) in whole.char_indices() {
            let _ = tracks(&whole[..cut]);
        }
        let before = whole.find("</Representation>").unwrap();
        assert_eq!(tracks(&whole[..before]), None);
        assert_eq!(tracks(""), None);
        assert_eq!(tracks("not a manifest"), None);
    }

    #[test]
    fn a_manifest_past_the_bound_is_not_read() {
        let mut long = manifest(&[representation("video/mp4", 1080, 3_000, "v")]);
        long.push_str(&" ".repeat(MAX_MANIFEST_BYTES));
        assert_eq!(tracks(&long), None);
        let many: Vec<String> = (0..=MOST_REPRESENTATIONS)
            .map(|i| representation("video/mp4", 720, i as u64, &format!("v{i}")))
            .collect();
        assert_eq!(tracks(&manifest(&many)), None);
    }

    #[test]
    fn a_representation_with_no_address_is_skipped() {
        let text = manifest(&[
            representation("video/mp4", 1080, 3_000, "v")
                .replace("https://x.fbcdn.net/v.mp4?a=1&amp;b=2", ""),
            representation("video/mp4", 720, 900, "w"),
        ]);
        assert_eq!(tracks(&text).unwrap().video.width, Some(720));
    }
}
