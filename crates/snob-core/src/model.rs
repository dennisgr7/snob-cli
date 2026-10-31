//! Domain types.
//!
//! Deliberately separate from the models in `snob-ig`, which are the wire
//! format and change whenever Instagram changes. What lives here is what gets
//! stored and what goes out as JSON, which is a contract with whoever consumes
//! the tool. Were they the same type, an Instagram change would break that
//! contract without anyone noticing.

use serde::{Deserialize, Serialize};

use crate::Pk;

/// An Instagram account as we store it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub pk: Pk,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_private: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pfp_url: Option<String>,
}

/// What may sit in a path segment unescaped: RFC 3986's unreserved set, which
/// contains every character Instagram lets a username be made of.
const IN_A_PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A name, ready to be one segment of a URL path.
///
/// Public for the same reason [`printable`] is: a username reaches an address
/// from more than one place, and [`User::profile_url`] is not all of them. The
/// `Referer` header the Instagram client sends names the page a browser would
/// have called from, which is built out of the username as typed — and a
/// header value cannot hold a byte below 0x20 at all, so an unencoded name
/// there does not produce a wrong address, it produces no request.
///
/// Encoded rather than filtered, for the reason [`User::profile_url`] gives at
/// length: removing a character from a name yields the address of a different
/// account.
pub fn in_a_path(name: &str) -> String {
    percent_encoding::utf8_percent_encode(name, IN_A_PATH).to_string()
}

impl User {
    /// An account known by its id and name alone, every other field empty.
    ///
    /// Nothing is invented: the metadata arrives with a walk, and an upsert
    /// leaves whatever it already had alone.
    pub fn named(pk: Pk, username: impl Into<String>) -> Self {
        Self {
            pk,
            username: username.into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    /// The address of this account, with the name encoded into it.
    ///
    /// **Encoded rather than filtered, and the reason is not the terminal.**
    /// This string lands in the URL slot of an OSC 8 sequence, in a markdown
    /// link destination and in a spreadsheet hyperlink, and [`printable`] would
    /// keep all three safe — but it *removes* characters, and a name with one
    /// removed is the address of a **different account**, which may well exist.
    /// A link that quietly points at somebody else is worse than one that does
    /// not work, because nothing on screen says anything happened.
    ///
    /// Percent-encoding cannot do that. It is the identity on the alphabet
    /// Instagram allows, so ordinary names come out byte for byte, and anything
    /// else round-trips instead of resolving elsewhere. It also keeps an `ESC`
    /// in a username from ending the OSC 8 sequence early and letting the rest
    /// of the name open one of its own, pointing wherever the name said.
    pub fn profile_url(&self) -> String {
        format!("https://www.instagram.com/{}/", in_a_path(&self.username))
    }

    /// The full name with control characters taken out, for anything a
    /// terminal will interpret.
    pub fn safe_full_name(&self) -> Option<String> {
        self.full_name.as_deref().map(printable)
    }

    /// The username, likewise.
    pub fn safe_username(&self) -> String {
        printable(&self.username)
    }
}

/// Strips what a terminal would obey rather than show.
///
/// Every string here came from another person's profile, and an account name is
/// the cheapest way into this program: anyone who follows you gets one. Left
/// alone, `\x1b]8;;http://…\x1b\\` in a full name turns a row into a link
/// somewhere else, `\x1b[2K\x1b[A` erases the line above, and on terminals with
/// OSC 52 enabled the clipboard can be written. None of that needs Instagram to
/// be compromised.
///
/// It removes rather than escapes: what is wanted is the name, and a name has
/// no control characters in it. Everything printable survives untouched,
/// accents and other scripts included.
///
/// The one exception is whitespace that happens to be a control character —
/// a newline or a tab. Those become a space rather than vanishing, because
/// deleting them would join two words that were never one.
///
/// Public because the same filter is needed for strings that never become a
/// [`User`]: the name a `scan` summary opens with, an account named in an
/// error, an excerpt of a response body. A second, slightly different copy of
/// this list is how one output path ends up covered and another does not.
///
/// **A variation selector survives only where it can mean something.**
/// U+FE00–U+FE0F draw nothing, and one after each letter of a name, or a run
/// of them after any character, is the known way to hide four bits or a byte
/// per selector inside text that looks ordinary: read by a model, that is an
/// instruction nobody on screen sees. So U+FE00–U+FE0D, which only a handful
/// of mathematical symbols and CJK compatibility ideographs use, are removed
/// outright, and U+FE0E/U+FE0F (text or emoji presentation; U+FE0F is what
/// asks for an emoji drawn in color) are kept only alone, right after a
/// character that can take a presentation ([`takes_a_presentation`]): one
/// in the ranges where Unicode puts the bases of those sequences (symbols,
/// arrows, dingbats, the emoji blocks), or a keycap base (`0`-`9`, `#`,
/// `*`) when U+20E3 follows and the keycap is drawn; never a letter, a
/// digit, a combining mark of any script, whitespace, or punctuation but
/// the four marks that are emoji themselves (U+203C, U+2049, U+3030,
/// U+303D). What is left hides at most one bit per visible symbol, and
/// nothing in a name written in letters, marks and digits. Judged on what
/// has been kept, so a selector cannot slip in behind a removed character.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => out.push(' '),
            c if c.is_control() || is_invisible(c) => {}
            '\u{fe0e}' | '\u{fe0f}' => {
                let keycap = chars.peek() == Some(&'\u{20e3}');
                if out
                    .chars()
                    .next_back()
                    .is_some_and(|base| takes_a_presentation(base, keycap))
                {
                    out.push(c);
                }
            }
            '\u{fe00}'..='\u{fe0d}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Whether `c` can be the base of a text or emoji presentation sequence,
/// the only place [`printable`] keeps U+FE0E or U+FE0F; `keycap` is whether
/// U+20E3 comes right after the selector.
///
/// **A positive list, on purpose**: the ranges where
/// `emoji-variation-sequences.txt` puts every base it names (some 350
/// characters), taken whole rather than character by character, so the
/// table need not follow every Unicode release. No letter of any script,
/// no decimal digit and no combining mark is in them, and of punctuation
/// only four marks that are emoji bases themselves (U+203C, U+2049, U+3030,
/// U+303D), so a name written in any script's letters, marks stacked on
/// them included (a Devanagari virama, a Thai tone mark, the kana voiced
/// mark of text in NFD), can hide nothing; no selector is, so a run keeps
/// at most one; and U+20E3 is not, so a keycap's mark drawn again over the
/// same keycap carries none. The symbols in them that Unicode counts as
/// letters or numbers (U+2139 INFORMATION SOURCE, U+24C2, the enclosed
/// letters and digits from U+1F100) are symbols to a reader, and several
/// are emoji bases (U+2139, U+24C2, the squared A, B, O and P), so they
/// keep their selector and are drawn in color. The keycap bases are ASCII
/// and qualify only before U+20E3 (`1` U+FE0F U+20E3). Every character in
/// the ranges is drawn as a symbol of its own, so what is kept costs a
/// reader at most one invisible bit per visible symbol.
fn takes_a_presentation(c: char, keycap: bool) -> bool {
    match c {
        '0'..='9' | '#' | '*' => keycap,
        '\u{a9}'
        | '\u{ae}'
        | '\u{203c}'
        | '\u{2049}'
        | '\u{2122}'
        | '\u{2139}'
        | '\u{2194}'..='\u{21aa}'
        // Around U+2329 and U+232A, angle brackets, and below the dingbat
        // brackets U+2768 to U+2775: punctuation that is no emoji base.
        | '\u{231a}'..='\u{2328}'
        | '\u{232b}'..='\u{23fa}'
        | '\u{24c2}'
        | '\u{25aa}'..='\u{25fe}'
        | '\u{2600}'..='\u{2767}'
        | '\u{2776}'..='\u{27bf}'
        | '\u{2934}'..='\u{2935}'
        | '\u{2b05}'..='\u{2b55}'
        | '\u{3030}'
        | '\u{303d}'
        | '\u{3297}'
        | '\u{3299}'
        | '\u{1f000}'..='\u{1faff}' => true,
        _ => false,
    }
}

/// The characters that show nothing but change how their neighbors read.
///
/// `char::is_control` covers general category `Cc`, which is both the C0 and
/// the C1 ranges. It does **not** cover these: they are category `Cf`, formatting
/// marks rather than controls, and a right-to-left override inside a name can
/// make one account read as another entirely.
///
/// Four groups, and the reason each is here:
///
/// - **Bidirectional overrides** — U+061C, the marks and the embedding,
///   override and isolate controls. This is the "Trojan Source" class, and
///   reversing the visible order of a name is the whole attack.
/// - **Zero-width characters** — ZWSP, ZWNJ, ZWJ, the word joiner, the
///   invisible operators and the byte-order mark. They let two different
///   accounts render as the same name, which is what makes a list of who to
///   unfollow untrustworthy.
/// - **The tag block**, U+E0000–U+E007F, which encodes arbitrary invisible
///   ASCII inside a name and renders as nothing at all, and **the
///   supplementary variation selectors**, U+E0100–U+E01EF, which carry the
///   same attack one byte per selector. They are category `Mn`, not `Cf`, and
///   only ideographic variation sequences use them, which a name loses nothing
///   by dropping. The rest of plane 14, to U+E0FFF, goes with them: it is
///   unassigned, and set aside to be drawn as nothing. So do **the Mongolian
///   free variation selectors**, U+180B–U+180D and U+180F, `Mn` as well and
///   drawn as nothing, which may follow any letter as often as anyone likes;
///   a Mongolian letter they would shape is drawn in its plain form, and
///   nothing else is lost. (Of the standard sixteen, [`printable`] keeps
///   U+FE0E and U+FE0F alone after an emoji base, and nothing else.)
/// - **Blank by rendering rather than by category** — the soft hyphen, the
///   combining grapheme joiner (U+034F, a mark, `Mn`), the Khmer inherent
///   vowels (U+17B4–U+17B5, marks too), the Mongolian vowel separator, the
///   interlinear annotation marks and the unassigned U+FFF0–U+FFF8 before
///   them, and the Hangul fillers, which are *letters* (`Lo`) that draw
///   nothing. No property this function could ask about groups them:
///   `White_Space` does not list them and `Cc` does not contain them, so
///   each one is another way to spell a name that is already taken.
///
/// With the standard variation selectors, which [`printable`] judges itself,
/// that is the whole of Unicode's `Default_Ignorable_Code_Point`: what a
/// renderer is told to draw as nothing when it does not know it.
///
/// The word-joiner range runs to U+206F rather than stopping at the invisible
/// operators, because the deprecated formatting characters between them are
/// invisible on the same terms.
///
/// **The whole of `Cf`, because AGENTS.md says so.** `char::is_control` is
/// `Cc` alone, so this list is the entire format-character coverage, including
/// the less familiar third: the Arabic number signs (U+0600–U+0605, U+06DD,
/// U+070F, U+0890–U+0891, U+08E2), the Kaithi and Egyptian-hieroglyph format
/// marks, the shorthand controls and the musical-symbol controls. A name is
/// `[a-z0-9._]`, so only `full_name` can carry one, and the cost is a display
/// name drawn shorter than it is — but the rules table states the guarantee
/// flatly, and a reader believes it.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00ad}'
        | '\u{034f}'
        | '\u{0600}'..='\u{0605}'
        | '\u{061c}'
        | '\u{06dd}'
        | '\u{070f}'
        | '\u{0890}'..='\u{0891}'
        | '\u{08e2}'
        | '\u{115f}'..='\u{1160}'
        | '\u{17b4}'..='\u{17b5}'
        | '\u{180b}'..='\u{180f}'
        | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{206f}'
        | '\u{3164}'
        | '\u{feff}'
        | '\u{ffa0}'
        | '\u{fff0}'..='\u{fffb}'
        | '\u{110bd}'
        | '\u{110cd}'
        | '\u{13430}'..='\u{1343f}'
        | '\u{1bca0}'..='\u{1bca3}'
        | '\u{1d173}'..='\u{1d17a}'
        | '\u{e0000}'..='\u{e0fff}')
}

/// Which side of the follow relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Followers,
    Following,
}

impl ListKind {
    /// How it is stored and how it appears in the API path.
    ///
    /// **Frozen**: this string feeds `snapshots.kind`, whose CHECK constraint
    /// only accepts these two values.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Followers => "followers",
            Self::Following => "following",
        }
    }
}

/// The other direction, for reading a stored row back.
///
/// The standard trait rather than an inherent `from_str`, which clippy warns
/// about for a good reason: a caller reaching for `"followers".parse()` would
/// otherwise get a different function than the one they expected, or none.
///
/// It fails rather than defaulting. A `kind` column holding something else
/// means the database was written by something that is not this program, and
/// quietly calling that "followers" would answer a question about a list nobody
/// asked about.
impl std::str::FromStr for ListKind {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "followers" => Ok(Self::Followers),
            "following" => Ok(Self::Following),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for ListKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a walk ended. Determines whether the snapshot can be compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The whole list was walked.
    Completed,
    /// The user interrupted it.
    Canceled,
    /// A caller-requested page cap was hit.
    PageLimit,
    /// Instagram stopped serving pages early.
    Truncated,
    /// Instagram is throttling requests.
    RateLimit,
    /// A network failure that did not recover.
    Network,
    /// The session stopped working.
    SessionInvalid,
}

impl StopReason {
    /// Whether the resulting snapshot can be used as the basis of a comparison.
    ///
    /// Only a full walk describes the whole list. With any other ending
    /// accounts are missing, and using it as a basis would make every one of
    /// them look like it left: the classic failure of tools like this one.
    ///
    /// A caller-requested cap is not an exception. The list is still short.
    pub fn yields_complete_list(self) -> bool {
        matches!(self, Self::Completed)
    }

    /// How it is stored in `snapshots.stopped_by`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Canceled => "canceled",
            Self::PageLimit => "page_limit",
            Self::Truncated => "truncated",
            Self::RateLimit => "rate_limit",
            Self::Network => "network",
            Self::SessionInvalid => "session_invalid",
        }
    }

    /// Every variant, so schema and code cannot drift apart untested.
    pub const ALL: [Self; 7] = [
        Self::Completed,
        Self::Canceled,
        Self::PageLimit,
        Self::Truncated,
        Self::RateLimit,
        Self::Network,
        Self::SessionInvalid,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(username: &str) -> User {
        User::named(Pk::new(1), username)
    }

    /// Encoding has to be the identity on the alphabet Instagram allows, or
    /// every link in every export changes for no reason.
    #[test]
    fn an_ordinary_name_comes_out_of_the_link_unchanged() {
        assert_eq!(
            named("some.user_1-x").profile_url(),
            "https://www.instagram.com/some.user_1-x/"
        );
    }

    /// The address goes into the URL slot of an OSC 8 sequence and into a
    /// markdown link destination, and an unencoded `ESC` there would end the
    /// sequence early: the cell would show a filtered name and point wherever
    /// the unfiltered one said.
    #[test]
    fn a_name_cannot_break_out_of_the_link_it_is_put_in() {
        let url = named("a\x1b\\\x1b]8;;http://evil.test\x1b\\Official").profile_url();

        assert!(!url.contains('\x1b'), "an escape survived: {url}");
        assert!(!url.contains(']'), "a sequence introducer survived: {url}");
        assert!(
            !url.contains("evil.test/"),
            "the second address is still a path of its own: {url}"
        );
    }

    /// Why this encodes instead of filtering. `printable` would remove the
    /// slashed o, and `/sren/` is somebody else's account — one that may well
    /// exist. A link quietly pointing at the wrong person is worse than a
    /// broken one, because nothing says it happened.
    ///
    /// The letter is Danish rather than Spanish so that the language guard has
    /// nothing to say about it. Which letter it is does not matter here; that
    /// it is not ASCII does.
    #[test]
    fn an_accent_still_points_at_the_same_account() {
        assert_eq!(
            named("søren").profile_url(),
            "https://www.instagram.com/s%C3%B8ren/"
        );
    }

    #[test]
    fn only_a_full_walk_yields_a_complete_list() {
        assert!(StopReason::Completed.yields_complete_list());
        for partial in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Truncated,
            StopReason::RateLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
        ] {
            assert!(
                !partial.yields_complete_list(),
                "{partial:?} does not describe the whole list"
            );
        }
    }

    /// The three groups the filter names, each with the character that makes
    /// the case: a bidirectional override, a zero-width space, and a tag
    /// character. None of them show anything, and all of them change what the
    /// name next to them means.
    #[test]
    fn invisible_characters_do_not_survive() {
        for hidden in [
            '\u{061c}',  // Arabic letter mark
            '\u{200b}',  // zero-width space
            '\u{200d}',  // zero-width joiner
            '\u{202e}',  // right-to-left override
            '\u{2060}',  // word joiner
            '\u{2068}',  // first strong isolate
            '\u{feff}',  // byte-order mark
            '\u{e0041}', // tag latin capital A
        ] {
            let name = format!("real{hidden}name");
            assert_eq!(
                printable(&name),
                "realname",
                "U+{:04X} survived",
                hidden as u32
            );
        }
    }

    /// The rest of what shows nothing. Category `Cc` covers none of these and
    /// the three groups above named none of them, so each one is a second way
    /// to write a name that already exists.
    #[test]
    fn the_other_invisible_characters_do_not_survive_either() {
        for hidden in [
            '\u{00ad}', // soft hyphen
            '\u{115f}', // Hangul choseong filler
            '\u{1160}', // Hangul jungseong filler
            '\u{180e}', // Mongolian vowel separator
            '\u{206a}', // inhibit symmetric swapping
            '\u{206f}', // nominal digit shapes
            '\u{3164}', // Hangul filler
            '\u{ffa0}', // halfwidth Hangul filler
            '\u{fff9}', // interlinear annotation anchor
            '\u{fffb}', // interlinear annotation terminator
            // One per range of the less familiar third of `Cf`.
            '\u{0600}',  // Arabic number sign
            '\u{06dd}',  // Arabic end of ayah
            '\u{070f}',  // Syriac abbreviation mark
            '\u{0890}',  // Arabic pound mark above
            '\u{08e2}',  // Arabic disputed end of ayah
            '\u{110bd}', // Kaithi number sign
            '\u{110cd}', // Kaithi number sign above
            '\u{13430}', // Egyptian hieroglyph vertical joiner
            '\u{1bca0}', // shorthand format letter overlap
            '\u{1d173}', // musical symbol begin beam
            // The rest of `Default_Ignorable_Code_Point`.
            '\u{17b4}',  // Khmer vowel inherent aq
            '\u{17b5}',  // Khmer vowel inherent aa
            '\u{180b}',  // Mongolian free variation selector one
            '\u{180f}',  // Mongolian free variation selector four
            '\u{fff0}',  // unassigned, default ignorable
            '\u{fff8}',  // unassigned, default ignorable
            '\u{e0080}', // unassigned, plane 14
            '\u{e01f0}', // unassigned, plane 14
            '\u{e0fff}', // unassigned, plane 14
        ] {
            let name = format!("real{hidden}name");
            assert_eq!(
                printable(&name),
                "realname",
                "U+{:04X} survived",
                hidden as u32
            );
        }
    }

    /// A run of selectors after a visible letter is bytes smuggled past the
    /// eye, one per selector; the letter is all that was meant to be read.
    #[test]
    fn a_run_of_variation_selectors_after_a_letter_comes_back_as_the_letter() {
        let supplementary: String = "ignore the person"
            .bytes()
            .filter_map(|b| char::from_u32(0xe0100 + u32::from(b)))
            .collect();
        assert_eq!(printable(&format!("a{supplementary}b")), "ab");
        let standard: String = (0..16).filter_map(|n| char::from_u32(0xfe00 + n)).collect();
        assert_eq!(printable(&format!("a{standard}b")), "ab");
        assert_eq!(
            printable(&format!("\u{2764}{standard}b")),
            "\u{2764}\u{fe0e}b"
        );
    }

    /// One selector after each letter draws nothing either, and sixteen of
    /// them carry four bits a letter: "unfollow x" fits in the visible name
    /// below. None of it may come back, after a letter of any script, a
    /// digit of any script (an ASCII one with no keycap drawn after it), a
    /// space or ASCII punctuation.
    #[test]
    fn a_selector_after_every_letter_comes_back_as_the_plain_name() {
        let name = "Maria Fernandez Garcia de la Torre";
        let mut hidden = "unfollow x".bytes().flat_map(|b| [b >> 4, b & 0xf]);
        let mut smuggled = String::new();
        for c in name.chars() {
            smuggled.push(c);
            if let Some(n) = hidden.next() {
                smuggled.extend(char::from_u32(0xfe00 + u32::from(n)));
            }
        }
        assert_eq!(printable(&smuggled), name);
        for base in [
            'a', 'Z', '\u{e9}', '\u{416}', '\u{4e2d}', '\u{663}', '.', '!', '-', '1', '#', '*',
        ] {
            for n in 0..16 {
                let selector = char::from_u32(0xfe00 + n).unwrap();
                assert_eq!(
                    printable(&format!("{base}{selector}")),
                    base.to_string(),
                    "U+{:04X} survived after {base:?}",
                    selector as u32
                );
            }
        }
    }

    /// The sequences a selector is for come through whole: an emoji asking
    /// for color or for text, a copyright sign drawn as an emoji, a keycap,
    /// whose base is an ASCII digit or `#`, and the emoji whose base Unicode
    /// counts as a letter (the information sign, the circled M, the squared
    /// A and P).
    #[test]
    fn real_presentation_sequences_come_back_whole() {
        for real in [
            "\u{2764}\u{fe0f}",
            "\u{2764}\u{fe0e}",
            "\u{a9}\u{fe0f}",
            "1\u{fe0f}\u{20e3}",
            "#\u{fe0f}\u{20e3}",
            "Ana \u{2600}\u{fe0f} Sol",
            "\u{2139}\u{fe0f}",
            "\u{24c2}\u{fe0f}",
            "\u{1f170}\u{fe0f}",
            "\u{1f17f}\u{fe0f}",
        ] {
            assert_eq!(printable(real), real);
        }
    }

    /// One U+FE0F after an emoji is how it asks to be drawn in color, so it
    /// stays; a second one selects nothing and goes, and so does one that
    /// opens the text or that a removed character stood between.
    #[test]
    fn an_emoji_keeps_its_one_presentation_selector_and_no_more() {
        assert_eq!(printable("\u{2764}\u{fe0f}"), "\u{2764}\u{fe0f}");
        assert_eq!(
            printable("\u{2764}\u{fe0f}\u{fe0f}\u{fe0f}"),
            "\u{2764}\u{fe0f}"
        );
        assert_eq!(printable("\u{fe0f}ana"), "ana");
        assert_eq!(
            printable("\u{2764}\u{fe0f}\u{200b}\u{fe0f}"),
            "\u{2764}\u{fe0f}"
        );
    }

    /// The combining grapheme joiner draws nothing either, though it is a mark
    /// (`Mn`) and not a format character.
    #[test]
    fn the_combining_grapheme_joiner_does_not_survive() {
        assert_eq!(printable("real\u{034f}name"), "realname");
    }

    /// The Mongolian free variation selectors draw nothing and may follow
    /// any letter, two bits each: "unfollow x" fits behind one letter.
    #[test]
    fn a_run_of_mongolian_variation_selectors_comes_back_as_the_letter() {
        let fvs = ['\u{180b}', '\u{180c}', '\u{180d}', '\u{180f}'];
        let smuggled: String = "unfollow x"
            .bytes()
            .flat_map(|b| [b >> 6, (b >> 4) & 3, (b >> 2) & 3, b & 3])
            .map(|n| fvs[usize::from(n)])
            .collect();
        assert_eq!(printable(&format!("Maria{smuggled}")), "Maria");
    }

    /// A combining mark is no base for a selector: not one stacked on a
    /// letter, which would hide a bit per letter of a plain Latin name, nor
    /// the marks of ordinary Hindi, Thai and Japanese in NFD (a virama, a
    /// tone mark, the voiced mark), nor a run of marks stacked on one
    /// letter, and not a keycap's U+20E3 drawn again over the same keycap.
    #[test]
    fn a_selector_after_a_combining_mark_does_not_survive() {
        assert_eq!(printable("a\u{301}\u{fe0f}"), "a\u{301}");
        assert_eq!(printable("e\u{20dd}\u{fe0e}"), "e\u{20dd}");
        assert_eq!(printable("a\u{94d}\u{fe0f}"), "a\u{94d}");
        assert_eq!(printable("\u{e01}\u{e48}\u{fe0f}"), "\u{e01}\u{e48}");
        assert_eq!(printable("\u{304b}\u{3099}\u{fe0e}"), "\u{304b}\u{3099}");
        let stacked: String = (0..40)
            .flat_map(|i| ['\u{951}', if i % 2 == 0 { '\u{fe0e}' } else { '\u{fe0f}' }])
            .collect();
        let marks = "\u{951}".repeat(40);
        assert_eq!(
            printable(&format!("Maria{stacked}")),
            format!("Maria{marks}")
        );
        assert_eq!(
            printable("1\u{fe0f}\u{20e3}\u{fe0f}\u{20e3}\u{fe0e}\u{20e3}\u{fe0f}"),
            "1\u{fe0f}\u{20e3}\u{20e3}\u{20e3}"
        );
    }

    /// Brackets that sit among emoji bases are punctuation, not bases, and
    /// keep no selector; the bases on either side of them still do.
    #[test]
    fn brackets_among_the_emoji_bases_keep_no_selector() {
        for bracket in ['\u{2329}', '\u{232a}', '\u{2768}', '\u{2775}'] {
            assert_eq!(
                printable(&format!("{bracket}\u{fe0f}")),
                bracket.to_string()
            );
        }
        for base in ['\u{2328}', '\u{23cf}', '\u{2764}', '\u{2795}'] {
            let kept = format!("{base}\u{fe0f}");
            assert_eq!(printable(&kept), kept);
        }
    }

    /// An ASCII digit, `#` or `*` keeps a selector only for a keycap drawn
    /// after it, so the digits of a phone number or a year hide nothing.
    #[test]
    fn a_digit_keeps_a_selector_only_before_a_keycap() {
        assert_eq!(
            printable("5\u{fe0f}5\u{fe0e}5\u{fe0f}1\u{fe0e}2\u{fe0f}"),
            "55512"
        );
        assert_eq!(printable("a1\u{fe0f}"), "a1");
        assert_eq!(printable("1\u{fe0f}\u{fe0f}\u{20e3}"), "1\u{fe0f}\u{20e3}");
        assert_eq!(printable("*\u{fe0f}\u{20e3}"), "*\u{fe0f}\u{20e3}");
    }

    /// Two accounts must not be able to render as the same name. This is the
    /// point of taking the zero-width characters out, rather than only the
    /// ones that drive the terminal.
    #[test]
    fn a_zero_width_character_cannot_forge_a_name() {
        let impostor = "insta\u{200b}gram";
        assert_ne!(impostor, "instagram");
        assert_eq!(printable(impostor), "instagram");
    }

    /// Taking the invisible characters out must not take the visible ones with
    /// them. Accents, other scripts and emoji are ordinary content.
    #[test]
    fn ordinary_text_is_untouched() {
        for name in ["Jos\u{e9} Mu\u{f1}oz", "\u{4e2d}\u{6587}", "ana \u{1f600}"] {
            assert_eq!(printable(name), name);
        }
    }

    #[test]
    fn a_user_serializes_without_its_empty_fields() {
        let u = User {
            is_verified: Some(true),
            ..User::named(Pk::new(42), "someone")
        };
        let json = serde_json::to_string(&u).unwrap();
        assert_eq!(json, r#"{"pk":42,"username":"someone","is_verified":true}"#);
    }

    /// Frozen: this string feeds `snapshots.kind`, and its CHECK constraint
    /// accepts nothing else.
    #[test]
    fn the_list_kind_wire_value_is_lowercase_english() {
        assert_eq!(ListKind::Followers.as_str(), "followers");
        assert_eq!(ListKind::Following.as_str(), "following");
    }

    #[test]
    fn every_stop_reason_has_a_distinct_stored_value() {
        let mut seen = std::collections::HashSet::new();
        for reason in StopReason::ALL {
            assert!(seen.insert(reason.as_str()), "{reason:?} is duplicated");
        }
        assert_eq!(seen.len(), 7);
    }
}
