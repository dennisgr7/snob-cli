//! The values the web client reads from the document it loaded, and sends
//! back with its own calls.
//!
//! A logged-in document defines them as modules, `["Name",[],{…},id]`,
//! inside the JSON the page bootstraps from. They are copied from the page,
//! never made up: a value the document did not carry stays `None`, and a
//! call that needs it is not sent. Several change with every load (`__hsi`,
//! `__spin_t`, the Relay token), so they are read again from each document
//! the tab loads, never kept across loads.
//!
//! The web session id is the exception: the app makes it in the page and
//! sends it with its calls, as `X-Web-Session-ID` and as the `__s` field, so
//! it is taken from those calls ([`web_session_id`]) rather than from the
//! document.

use serde_json::Value;
use snob_core::Pk;
use snob_core::secret::Secret;

/// The modules read. The browser engine hands back the text from the first
/// definition of each, for [`PageValues::from_definitions`].
pub const DEFINED: [&str; 6] = [
    "PolarisViewer",
    "SiteData",
    "LSD",
    "DTSGInitialData",
    "DTSGInitData",
    "WebBloksVersioningID",
];

/// Where a module's definition starts. The bracket, the quotes and the empty
/// dependency list are part of it: a bare name is also the start of longer
/// ones (`LSD` of `LSDatabaseSingletonLazyWrapper`).
pub fn marker(name: &str) -> String {
    format!("[\"{name}\",[],")
}

/// Who the document was served to: `PolarisViewer`, which is
/// `{data: null, id: null}` on a logged-out page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Viewer {
    pub pk: Pk,
    pub username: String,
    /// The Facebook-side id Relay sends as `av`. Not the pk.
    pub fbid: String,
}

/// The build and page context the document was served in: the **first**
/// `SiteData`. A document defines it twice, with different `__spin_t`, and
/// the app sends the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// `client_revision`, sent as `__rev` and as `X-Instagram-AJAX`.
    pub revision: String,
    /// `hsi`, sent as `__hsi`.
    pub hsi: String,
    /// `haste_session`, sent as `__hs`.
    pub haste_session: String,
    pub spin_r: String,
    pub spin_b: String,
    pub spin_t: String,
}

/// Everything read from one document, plus the web session id its app sent.
#[derive(Debug, Clone, Default)]
pub struct PageValues {
    pub viewer: Option<Viewer>,
    pub site: Option<Site>,
    /// `LSD`, sent as `lsd` and `X-FB-LSD`.
    pub lsd: Option<Secret>,
    /// `DTSGInitialData`'s token, taken for the per-load one Relay and
    /// Comet POSTs carry. **Unconfirmed**: the capture does not say which of
    /// the two modules that is, so a call is built with it only when the app
    /// itself sent it on this document ([`crate::web::AppCalls`]). Empty, so
    /// `None`, on a logged-out page.
    pub relay_dtsg: Option<Secret>,
    /// `DTSGInitData`'s token. Which of the app's calls carry it is
    /// unconfirmed, and no call is built with this field; the live write sends
    /// the same token as `fb_dtsg`, read through `graphql::extract_tokens`.
    pub session_dtsg: Option<Secret>,
    /// `WebBloksVersioningID`, sent as `X-Bloks-Version-Id` and `__bkv`.
    pub bloks_version: Option<String>,
    /// `X-Web-Session-ID` / `__s`, from the app's calls on this document.
    pub web_session: Option<String>,
    /// The document says nobody is logged in: `PolarisViewer` is defined
    /// with `id: null`, the marker a logged-out document was recorded with.
    /// A document that does not define it at all says nothing either way.
    pub signed_out: bool,
}

impl PageValues {
    /// Reads the values out of a whole document. Each module is taken from
    /// its first definition.
    pub fn parse(document: &str) -> Self {
        Self::read(|name| defined(document, name))
    }

    /// Reads the values out of what the engine hands back from a document:
    /// for each module of [`DEFINED`], the text from its first definition
    /// on. Each is read only from its own text, which can run on into a
    /// later definition of another module.
    pub fn from_definitions<'a>(definition: impl Fn(&str) -> Option<&'a str>) -> Self {
        Self::read(|name| defined(definition(name)?, name))
    }

    fn read(defined: impl Fn(&str) -> Option<Value>) -> Self {
        let token = |name| {
            defined(name)
                .and_then(|v| string(v.get("token")))
                .map(Secret::new)
        };
        let polaris_viewer = defined("PolarisViewer");
        Self {
            signed_out: polaris_viewer
                .as_ref()
                .is_some_and(|v| v.get("id").is_some_and(Value::is_null)),
            viewer: polaris_viewer.and_then(|v| viewer(&v)),
            site: defined("SiteData").and_then(|v| site(&v)),
            lsd: token("LSD"),
            relay_dtsg: token("DTSGInitialData"),
            session_dtsg: token("DTSGInitData"),
            bloks_version: defined("WebBloksVersioningID")
                .and_then(|v| string(v.get("versioningID"))),
            web_session: None,
        }
    }
}

/// A web session id as the app sends it: three segments of letters and
/// digits, the last of which changes with each page load. Anything else is
/// not taken.
pub fn web_session_id(value: &str) -> Option<&str> {
    let mut segments = 0;
    for segment in value.split(':') {
        if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        segments += 1;
    }
    (segments == 3).then_some(value)
}

/// The exported object of a module's first definition.
fn defined(text: &str, name: &str) -> Option<Value> {
    let marker = marker(name);
    let at = text.find(&marker)? + marker.len();
    serde_json::Deserializer::from_str(&text[at..])
        .into_iter::<Value>()
        .next()?
        .ok()
        .filter(Value::is_object)
}

fn viewer(value: &Value) -> Option<Viewer> {
    let data = value.get("data")?;
    Some(Viewer {
        pk: string(value.get("id"))?.parse().ok()?,
        username: string(data.get("username"))?,
        fbid: string(data.get("fbid"))?,
    })
}

fn site(value: &Value) -> Option<Site> {
    Some(Site {
        revision: string(value.get("client_revision"))?,
        hsi: string(value.get("hsi"))?,
        haste_session: string(value.get("haste_session"))?,
        spin_r: string(value.get("__spin_r"))?,
        spin_b: string(value.get("__spin_b"))?,
        spin_t: string(value.get("__spin_t"))?,
    })
}

/// A value as the form sends it: a string as it is, a number in decimal.
/// Empty and anything else is absent.
fn string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A logged-in document, shaped like the capture's with made-up values:
    /// two `SiteData` with different `__spin_t`, each token defined twice,
    /// the definitions inside the bootstrap JSON of `application/json`
    /// scripts, beside a module whose name starts like one of them.
    const LOGGED_IN: &str = r#"<!DOCTYPE html><html><head>
<script type="application/json" data-sjs>{"require":[["ScheduledServerJS","handle",null,[{"__bbox":{"define":[["LSDatabaseSingletonLazyWrapper",[],{"token":"WRONG"},1],["SiteData",[],{"server_revision":1000000001,"client_revision":1000000001,"tier":"","haste_session":"20000.HYP:instagram_web_pkg.2.1...0","hsi":"7000000000000000001","__spin_r":1000000001,"__spin_b":"trunk","__spin_t":1790000001},317],["LSD",[],{"token":"lsdTokenAAAAAAAAAAAAAA"},323],["DTSGInitialData",[],{"token":"relay:token:one"},258],["DTSGInitData",[],{"token":"session:token:one","async_get_token":"async:token:one"},3515],["WebBloksVersioningID",[],{"versioningID":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},6013]]}}]]]}</script>
</head><body>
<script type="application/json" data-sjs>{"require":[["ScheduledServerJS","handle",null,[{"__bbox":{"define":[["PolarisViewer",[],{"data":{"id":"1234567890","username":"some.viewer","full_name":"Some Viewer","fbid":"17841400000000001","is_private":true,"profile_pic_url":"https://example.invalid/a.jpg"},"id":"1234567890"},1508],["SiteData",[],{"client_revision":1000000001,"haste_session":"20000.HYP:instagram_web_pkg.2.1...0","hsi":"7000000000000000001","__spin_r":1000000001,"__spin_b":"trunk","__spin_t":1790000002},317],["DTSGInitialData",[],{"token":"relay:token:two"},258],["DTSGInitData",[],{"token":"session:token:two","async_get_token":"async:token:two"},3515]]}}]]]}</script>
</body></html>"#;

    #[test]
    fn a_logged_in_document_carries_every_value() {
        let values = PageValues::parse(LOGGED_IN);
        assert!(!values.signed_out);
        assert_eq!(
            values.viewer,
            Some(Viewer {
                pk: Pk::new(1_234_567_890),
                username: "some.viewer".into(),
                fbid: "17841400000000001".into(),
            })
        );
        assert_eq!(
            values.site,
            Some(Site {
                revision: "1000000001".into(),
                hsi: "7000000000000000001".into(),
                haste_session: "20000.HYP:instagram_web_pkg.2.1...0".into(),
                spin_r: "1000000001".into(),
                spin_b: "trunk".into(),
                spin_t: "1790000001".into(),
            })
        );
        assert_eq!(
            values.lsd.as_ref().map(Secret::expose),
            Some("lsdTokenAAAAAAAAAAAAAA")
        );
        assert_eq!(
            values.relay_dtsg.as_ref().map(Secret::expose),
            Some("relay:token:one")
        );
        assert_eq!(
            values.session_dtsg.as_ref().map(Secret::expose),
            Some("session:token:one")
        );
        assert_eq!(
            values.bloks_version.as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(values.web_session, None);
    }

    /// The app sends the first `SiteData`; the second differs in `__spin_t`.
    #[test]
    fn the_first_site_data_is_the_one_read() {
        let site = PageValues::parse(LOGGED_IN).site.unwrap();
        assert_eq!(site.spin_t, "1790000001");
    }

    /// A logged-out document: a viewer of nulls and an empty Relay token,
    /// neither of which is a value.
    #[test]
    fn a_logged_out_document_has_no_viewer_and_no_relay_token() {
        let logged_out = concat!(
            r#"{"define":[["PolarisViewer",[],{"data":null,"id":null},1508],"#,
            r#"["DTSGInitialData",[],{},258],"#,
            r#"["DTSGInitData",[],{"token":"session:token","async_get_token":"async"},3515],"#,
            r#"["LSD",[],{"token":"lsd"},323]]}"#
        );
        let values = PageValues::parse(logged_out);
        assert!(values.signed_out);
        assert_eq!(values.viewer, None);
        assert!(values.relay_dtsg.is_none());
        assert_eq!(values.lsd.as_ref().map(Secret::expose), Some("lsd"));
        assert!(values.session_dtsg.is_some());
    }

    /// A value the page did not carry is absent, not borrowed from the next
    /// module or made up.
    #[test]
    fn a_missing_value_stays_missing() {
        let values = PageValues::parse(r#"["LSD",[],{"token":"only"},323]"#);
        assert!(!values.signed_out, "no viewer defined says nothing");
        assert!(values.relay_dtsg.is_none());
        assert!(values.session_dtsg.is_none());
        assert!(values.site.is_none());
        assert!(values.viewer.is_none());
        assert!(values.bloks_version.is_none());

        let partial = r#"["SiteData",[],{"client_revision":1,"hsi":"2"},317]"#;
        assert!(PageValues::parse(partial).site.is_none());
    }

    /// A definition cut short, as a truncated read hands it back, is absent.
    #[test]
    fn a_definition_cut_short_is_absent() {
        let cut = r#"["PolarisViewer",[],{"data":{"id":"1234567890","username":"some.vi"#;
        assert!(PageValues::parse(cut).viewer.is_none());
        assert!(PageValues::parse("").viewer.is_none());
    }

    /// Ids arrive as strings in the capture; a number is read the same.
    #[test]
    fn a_viewer_id_written_as_a_number_is_read() {
        let viewer = r#"["PolarisViewer",[],{"data":{"username":"v","fbid":17841400000000001},"id":1234567890},1]"#;
        let viewer = PageValues::parse(viewer).viewer.unwrap();
        assert_eq!(viewer.pk, Pk::new(1_234_567_890));
        assert_eq!(viewer.fbid, "17841400000000001");
    }

    /// Read from what the engine hands back, each module from its own
    /// text: the viewer's runs on into the document's second `SiteData`,
    /// which is not the one the app sends.
    #[test]
    fn each_definition_is_read_from_its_own_text() {
        let viewer = concat!(
            r#"["PolarisViewer",[],{"data":{"username":"v","fbid":"17841400000000001"},"id":"1234567890"},1],"#,
            r#"["SiteData",[],{"client_revision":1,"hsi":"2","haste_session":"3","__spin_r":1,"__spin_b":"trunk","__spin_t":222},317]"#
        );
        let site = r#"["SiteData",[],{"client_revision":1,"hsi":"2","haste_session":"3","__spin_r":1,"__spin_b":"trunk","__spin_t":111},317]"#;
        let values = PageValues::from_definitions(|name| match name {
            "PolarisViewer" => Some(viewer),
            "SiteData" => Some(site),
            _ => None,
        });
        assert_eq!(values.viewer.unwrap().username, "v");
        assert_eq!(values.site.unwrap().spin_t, "111");
        assert!(values.lsd.is_none());
    }

    /// The tokens are credentials and are never printed.
    #[test]
    fn the_tokens_are_not_printed() {
        let printed = format!("{:?}", PageValues::parse(LOGGED_IN));
        for token in ["lsdToken", "relay:token", "session:token", "async:token"] {
            assert!(!printed.contains(token), "{printed}");
        }
    }

    #[test]
    fn a_web_session_id_is_three_segments_of_letters_and_digits() {
        assert_eq!(
            web_session_id("abc123:def456:ghi789"),
            Some("abc123:def456:ghi789")
        );
        for wrong in [
            "",
            "abc123:def456",
            "abc123:def456:ghi789:jkl012",
            "abc123::ghi789",
            "abc123:def 56:ghi789",
            "abc123:def456:ghi789\n",
        ] {
            assert_eq!(web_session_id(wrong), None, "{wrong:?}");
        }
    }
}
