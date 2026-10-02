//! A post's or a reel's address, and the media id it names.
//!
//! **The code in an address is the media's pk, written in base64.** Its first
//! eleven characters, read as base64url (`A-Z a-z 0-9 - _`, most significant
//! first), are the pk the app asks `media/<pk>/info/` with. Checked on the
//! captures of 2026-10-01 against every post and carousel item they carry,
//! the 39-character codes a private account's posts get included: those are
//! the same eleven characters with an opaque tail, and the tail is not needed.
//! So a link costs nothing to turn into what is asked; no request looks it up.
//!
//! **Only the path is kept.** A link copied from the app carries a query of
//! its own: where it was shared from, and a token that ties the visit to the
//! account that shared it. None of it is read and none of it is sent; the
//! code is all snob asks with, from the page it would open on, `/p/<code>/`
//! or `/reel/<code>/`. The document at that address is never loaded: a cold
//! load of a reel's link lands on the app's reels screen, which reports what
//! it plays.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A post's or a reel's id: the `pk` of `media/<pk>/info/`, which is never an
/// account's ([`snob_core::Pk`]), so it has a type of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MediaPk(u64);

impl MediaPk {
    pub const fn new(pk: u64) -> Self {
        Self(pk)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for MediaPk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for MediaPk {
    /// A number, or the digits of one as text, which is how every answer of
    /// the captures spells it.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u64),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Number(n) => Ok(Self(n)),
            Raw::Text(text) => text.parse().map(Self).map_err(D::Error::custom),
        }
    }
}

/// Which page of the app a code opens on: a post's, or a reel's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Opens {
    Post,
    Reel,
}

/// A post's or a reel's code, as an address or as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcode {
    code: String,
    opens: Opens,
}

/// Why a link names no post.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotALink {
    #[error("that is not an Instagram address")]
    Host,
    #[error("that address names no post or reel")]
    Path,
    #[error("that is not a post's code")]
    Code,
}

/// How many characters of a code hold the pk.
const PK_CHARS: usize = 11;

/// The longest code accepted: the longest the captures hold is 39, and a
/// code past this is not one Instagram hands out.
const LONGEST: usize = 64;

impl Shortcode {
    /// The code `input` names: a link to a post or a reel, with or without
    /// its scheme, or the bare code. The links the app gives out are
    /// `instagram.com/p/<code>/`, `/reel/<code>/` and `/reels/<code>/`, and a
    /// post opened from a profile's grid is `/<name>/p/<code>/`. The query and
    /// the fragment are dropped unread.
    pub fn parse(input: &str) -> Result<Self, NotALink> {
        let input = input.trim();
        if !input.contains('/') && !input.contains('.') {
            return Self::of(input, Opens::Post);
        }
        let with_scheme = if input.contains("://") {
            input.to_string()
        } else {
            format!("https://{input}")
        };
        let url = url::Url::parse(&with_scheme).map_err(|_| NotALink::Host)?;
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if !matches!(url.scheme(), "http" | "https")
            || !(host == "instagram.com" || host.ends_with(".instagram.com"))
        {
            return Err(NotALink::Host);
        }
        let segments: Vec<&str> = url
            .path_segments()
            .map(|s| s.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let (kind, code) = match segments.as_slice() {
            [kind, code] | [_, kind, code] => (*kind, *code),
            _ => return Err(NotALink::Path),
        };
        let opens = match kind {
            "p" | "tv" => Opens::Post,
            "reel" | "reels" => Opens::Reel,
            _ => return Err(NotALink::Path),
        };
        Self::of(code, opens)
    }

    fn of(code: &str, opens: Opens) -> Result<Self, NotALink> {
        let valid = (PK_CHARS..=LONGEST).contains(&code.len())
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !valid || pk_of(code).is_none() {
            return Err(NotALink::Code);
        }
        Ok(Self {
            code: code.to_string(),
            opens,
        })
    }

    /// The code, as written.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Which page of the app it opens on.
    pub fn opens(&self) -> Opens {
        self.opens
    }

    /// The media's pk ([`pk_of`]).
    pub fn pk(&self) -> MediaPk {
        // Checked when the code was accepted.
        pk_of(&self.code).unwrap_or(MediaPk(0))
    }

    /// The page the app opens it on, which is what it asks about it from:
    /// `/p/<code>/` for a post, `/reel/<code>/` for a reel.
    pub fn page(&self) -> String {
        page_of(&self.code, self.opens)
    }
}

/// The page the app opens `code` on, `/p/<code>/` or `/reel/<code>/`. The
/// code is base64url, which needs no escaping in a path.
pub fn page_of(code: &str, opens: Opens) -> String {
    match opens {
        Opens::Post => format!("/p/{code}/"),
        Opens::Reel => format!("/reel/{code}/"),
    }
}

/// The eleven characters that spell `pk` in base64url: the code of the
/// post, or its first eleven characters when the post is a private
/// account's, whose code goes on past them.
pub fn code_of(pk: MediaPk) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    (0..PK_CHARS)
        .rev()
        .map(|i| ALPHABET[((u128::from(pk.0) >> (6 * i)) & 63) as usize] as char)
        .collect()
}

/// The pk a code's first eleven characters spell in base64url, when they
/// spell one: a number that fits in 64 bits and is not zero.
pub fn pk_of(code: &str) -> Option<MediaPk> {
    let head = code.get(..PK_CHARS)?;
    let mut n: u128 = 0;
    for byte in head.bytes() {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        n = n * 64 + u128::from(digit);
    }
    u64::try_from(n).ok().filter(|n| *n != 0).map(MediaPk)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codes and pks the captures of 2026-10-01 pair: a post, a carousel's
    /// item, and a private account's long code, whose first eleven
    /// characters carry the pk.
    #[test]
    fn a_code_is_its_pk_in_base64() {
        for (code, pk) in [
            ("DTYHCKvDNxy", 3_807_824_420_233_075_826),
            ("DbGwql4IMei", 3_947_056_156_557_494_178),
            ("DdzD6j1xc7w", 3_995_554_512_760_065_776),
            (
                "DdzEShhEWCRnxelCuqsxG5vhMRx3FuF81XXoEM0",
                3_995_556_159_532_654_737,
            ),
        ] {
            assert_eq!(pk_of(code), Some(MediaPk::new(pk)), "{code}");
            assert_eq!(code_of(MediaPk::new(pk)), code[..PK_CHARS], "{code}");
        }
    }

    #[test]
    fn a_link_is_read_by_its_path_alone() {
        for (link, code, opens) in [
            (
                "https://www.instagram.com/p/DTYHCKvDNxy/",
                "DTYHCKvDNxy",
                Opens::Post,
            ),
            (
                "https://www.instagram.com/reel/DbGwql4IMei/?utm_source=ig_web_copy_link&igsh=x",
                "DbGwql4IMei",
                Opens::Reel,
            ),
            (
                "instagram.com/reels/DbGwql4IMei",
                "DbGwql4IMei",
                Opens::Reel,
            ),
            (
                "https://www.instagram.com/someone/p/DTYHCKvDNxy/?img_index=2#top",
                "DTYHCKvDNxy",
                Opens::Post,
            ),
            ("  DTYHCKvDNxy  ", "DTYHCKvDNxy", Opens::Post),
            (
                "http://m.instagram.com/tv/DTYHCKvDNxy/",
                "DTYHCKvDNxy",
                Opens::Post,
            ),
        ] {
            let parsed = Shortcode::parse(link).unwrap();
            assert_eq!(parsed.code(), code, "{link}");
            assert_eq!(parsed.opens(), opens, "{link}");
        }
        let reel = Shortcode::parse("https://www.instagram.com/reel/DbGwql4IMei/?a=b").unwrap();
        assert_eq!(reel.page(), "/reel/DbGwql4IMei/");
        assert_eq!(reel.pk(), MediaPk::new(3_947_056_156_557_494_178));
        let post = Shortcode::parse("DTYHCKvDNxy").unwrap();
        assert_eq!(post.page(), "/p/DTYHCKvDNxy/");
    }

    #[test]
    fn anything_else_names_no_post() {
        for (link, why) in [
            ("https://example.com/p/DTYHCKvDNxy/", NotALink::Host),
            (
                "https://instagram.com.example.com/p/DTYHCKvDNxy/",
                NotALink::Host,
            ),
            ("ftp://www.instagram.com/p/DTYHCKvDNxy/", NotALink::Host),
            ("https://www.instagram.com/someone/", NotALink::Path),
            (
                "https://www.instagram.com/stories/someone/1/",
                NotALink::Path,
            ),
            (
                "https://www.instagram.com/p/DTYHCKvDNxy/a/b/",
                NotALink::Path,
            ),
            ("https://www.instagram.com/p/short/", NotALink::Code),
            ("https://www.instagram.com/p/DTYHCKv%2FNxy/", NotALink::Code),
            ("DTYHCK", NotALink::Code),
            ("AAAAAAAAAAA", NotALink::Code),
            ("zzzzzzzzzzz", NotALink::Code),
        ] {
            assert_eq!(Shortcode::parse(link), Err(why.clone()), "{link}");
        }
    }

    #[test]
    fn a_pk_reads_from_a_number_or_its_digits() {
        let pk: MediaPk = serde_json::from_str("3807824420233075826").unwrap();
        assert_eq!(pk, MediaPk::new(3_807_824_420_233_075_826));
        let pk: MediaPk = serde_json::from_str(r#""3807824420233075826""#).unwrap();
        assert_eq!(pk.to_string(), "3807824420233075826");
        assert!(serde_json::from_str::<MediaPk>(r#""x""#).is_err());
    }
}
