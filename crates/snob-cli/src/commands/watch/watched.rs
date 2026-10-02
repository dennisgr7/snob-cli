//! Which accounts a run watches, and on whose say-so.
//!
//! One answer, for both modes and for `super::status`, which is what a file of
//! its own is for here: two answers could disagree about the accounts the file
//! names.

use snob_core::Pk;
use snob_store::config::WatchConfig;

use crate::engine::watch::Watched;

/// Which account a scheduled run watches, as whom, and whether it may.
///
/// A name on the command line is checked against the file, because that is the
/// only place a consent can have been recorded — and an unattended run that
/// could be pointed at a stranger by an argument would make the recording
/// pointless. It is read as `in_use`, the account the command resolved, and so
/// is an entry that names no viewer: that is what such an entry meant before
/// entries named one, and filling it in here keeps one account from being two
/// groups.
pub(super) fn watched_from(
    target: Option<String>,
    configured: Option<&WatchConfig>,
    in_use: Option<Pk>,
) -> Vec<Watched> {
    if let Some(name) = target {
        return vec![with_recorded_consent(&name, configured).read_as(in_use)];
    }

    let listed: Vec<Watched> = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .map(|account| {
            let watched = if account.is_own() {
                Watched::own()
            } else {
                with_recorded_consent(&account.target, configured)
            };
            watched.read_as(account.viewer.or(in_use))
        })
        .collect();

    // A file with no `[[account]]` at all means the obvious thing rather than
    // nothing: somebody who configured a schedule and a webhook and never
    // mentioned an account meant their own.
    if listed.is_empty() {
        vec![Watched::own().read_as(in_use)]
    } else {
        listed
    }
}

/// One account, with whatever answer is on record for it.
///
/// The file is the only place a consent can have come from, which is what makes
/// an unattended run safe: an argument cannot grant one.
///
/// **The at sign comes off here, on both sides**, which makes this the boundary
/// a `Watched` is built at, as every other reader in the tool cleans
/// (`target::resolve`, `target::from_store`, `target::label`). Otherwise a
/// typed `"@friend"` against a file recording an answer for `friend` would
/// match nothing and refuse a correctly consented monitor at startup, and a
/// hand-edited `target = "@friend"` would reach `engine::check` as
/// `username=@friend` and report a working configuration as broken. README.md
/// promises without qualification that a username may be written either way.
fn with_recorded_consent(name: &str, configured: Option<&WatchConfig>) -> Watched {
    let name = crate::engine::target::clean(name);
    let recorded = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .find(|account| {
            !account.is_own()
                && crate::engine::target::clean(&account.target).eq_ignore_ascii_case(name)
        })
        .and_then(|account| account.consent);

    if recorded.is_some() {
        // The table is what matters, not what is in it. `[account.consent]`
        // exists because somebody was asked; its `agreed_at` is the record of
        // when, kept in the file, and nothing downstream of here reads it or
        // could tell a real moment from whatever a hand-edit wrote.
        Watched::consented(name.to_string(), crate::engine::watch::Consent)
    } else {
        Watched::asking(name.to_string())
    }
}

/// What the opening line names, so somebody starting the service can see that
/// it understood which accounts it is for.
pub(super) fn watching_label(watched: &[Watched]) -> String {
    let names: Vec<String> = watched
        .iter()
        .map(|w| crate::app::target_label(w.name()))
        .collect();

    crate::report::and_list(&names).unwrap_or_else(|| "nothing".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::watch_toml;
    use crate::engine::watch::group_by_viewer;

    /// The opening line names what it watches as a sentence would.
    #[test]
    fn the_opening_line_names_what_it_watches() {
        let friend = |name: &str| Watched::consented(name.into(), crate::engine::watch::Consent);
        assert_eq!(watching_label(&[]), "nothing");
        assert_eq!(watching_label(&[Watched::own()]), "your account");
        assert_eq!(
            watching_label(&[Watched::own(), friend("a"), friend("b")]),
            "your account, @a and @b"
        );
    }

    /// A stranger is asked unless the file records that somebody answered.
    ///
    /// `with_recorded_consent` is the only thing standing between `--target`
    /// and a scheduled run enumerating somebody else's lists: make its `None`
    /// arm hand back a `Watched::consented` and an unattended run reads a
    /// stranger on nobody's say-so. A test of `may_run_unattended` on values
    /// built by hand never reaches the function that decides which you get.
    #[test]
    fn a_stranger_is_asked_unless_the_file_says_somebody_answered() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "self"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }

[[account]]
target = "acquaintance"
"#,
        );

        let asked = |name: &str, config: Option<&WatchConfig>| {
            let watched = watched_from(Some(name.to_string()), config, None);
            assert_eq!(watched.len(), 1);
            !watched[0].may_run_unattended()
        };

        assert!(
            !asked("friend", Some(&file)),
            "the file records an answer for them"
        );
        assert!(
            asked("stranger", Some(&file)),
            "nobody ever agreed to this one being read"
        );
        assert!(
            asked("acquaintance", Some(&file)),
            "listed is not the same as consented -- the answer is the `consent` table"
        );
        assert!(
            asked("friend", None),
            "with no file there is nowhere an answer could have been recorded"
        );

        // Instagram's spelling and the typed one need not agree in case.
        assert!(!asked("FRIEND", Some(&file)));

        // `self` on the command line is not the `[[account]] target = "self"`
        // line: that one is your own account, which needs nobody's permission,
        // and matching it would hand a stranger named `self` a consent.
        assert!(asked("self", Some(&file)));
    }

    /// The at sign a person types does not change which account is watched.
    ///
    /// Both spellings mean the same account and README.md says so without
    /// qualification.
    #[test]
    fn an_at_sign_does_not_change_which_account_is_watched() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }
"#,
        );

        let typed = watched_from(Some("@friend".to_string()), Some(&file), None);
        assert_eq!(typed.len(), 1);
        assert_eq!(
            typed[0].name(),
            Some("friend"),
            "what reaches the profile endpoint is a username, and the sign is not part of one"
        );
        assert!(
            typed[0].may_run_unattended(),
            "the file records an answer for this account, however it was spelled"
        );

        // And from the other side: the file is documented as safe to hand-edit,
        // so the sign can be in it instead.
        let edited = watch_toml(
            r#"
every = "6h"

[[account]]
target = "@friend"
consent = { agreed_at = 1700 }
"#,
        );
        let listed = watched_from(None, Some(&edited), None);
        assert_eq!(
            listed.iter().map(|w| w.name()).collect::<Vec<_>>(),
            vec![Some("friend")]
        );
        assert!(listed[0].may_run_unattended());

        // `@self` is the same line as `self`, and it is your own account. Read
        // as a stranger it would send a scheduled run looking for confirmation
        // to read an account it owns.
        let own = watch_toml("every = \"6h\"\n\n[[account]]\ntarget = \"@self\"\n");
        let listed = watched_from(None, Some(&own), None);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name(), None, "your own account names nobody");
        assert!(listed[0].may_run_unattended());
    }

    /// Every account the file lists is watched, and a file that lists none
    /// means your own.
    #[test]
    fn the_accounts_watched_are_the_ones_the_file_names() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "self"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }
"#,
        );

        let watched = watched_from(None, Some(&file), None);
        let names: Vec<Option<&str>> = watched.iter().map(|w| w.name()).collect();
        assert_eq!(names, [None, Some("friend")]);
        assert!(watched.iter().all(|w| w.may_run_unattended()));

        // A schedule with no `[[account]]` at all means the obvious thing.
        let bare = watch_toml("every = \"6h\"\n");
        let watched = watched_from(None, Some(&bare), None);
        assert_eq!(watched.len(), 1);
        assert_eq!(watched[0].name(), None);
    }

    /// Every entry is read as the viewer the file names, and one that names
    /// none -- or a name typed on the command line -- as the account in use.
    #[test]
    fn each_entry_is_read_as_its_viewer_or_the_account_in_use() {
        let file = watch_toml(
            r#"
schema = 2
every = "6h"

[[account]]
target = "self"
viewer = 1

[[account]]
target = "friend"
consent = { agreed_at = 1700 }

[[account]]
target = "self"
viewer = 2
"#,
        );
        let in_use = Some(Pk::new(3));

        let watched = watched_from(None, Some(&file), in_use);
        let viewers: Vec<Option<Pk>> = watched.iter().map(Watched::viewer).collect();
        assert_eq!(viewers, [Some(Pk::new(1)), in_use, Some(Pk::new(2))]);

        let typed = watched_from(Some("friend".to_string()), Some(&file), in_use);
        assert_eq!(
            typed[0].viewer(),
            in_use,
            "a typed name is read as the account resolved"
        );
        assert!(typed[0].may_run_unattended());

        let bare = watch_toml("every = \"6h\"\n");
        assert_eq!(watched_from(None, Some(&bare), in_use)[0].viewer(), in_use);

        // Nobody signed in: nothing to fill in, and the file's own still count.
        let alone = watched_from(None, Some(&file), None);
        assert_eq!(alone[1].viewer(), None);
        assert_eq!(alone[0].viewer(), Some(Pk::new(1)));
    }

    /// Groups come in the order the file first names each viewer, and each
    /// keeps its entries in the file's order.
    #[test]
    fn groups_keep_the_order_of_the_file() {
        let file = watch_toml(
            r#"
schema = 2
every = "6h"

[[account]]
target = "one"
viewer = 2
consent = { agreed_at = 1700 }

[[account]]
target = "self"
viewer = 1

[[account]]
target = "self"
viewer = 2

[[account]]
target = "two"
consent = { agreed_at = 1700 }
"#,
        );

        let groups = group_by_viewer(&watched_from(None, Some(&file), Some(Pk::new(1))));
        let shape: Vec<(Option<Pk>, Vec<Option<&str>>)> = groups
            .iter()
            .map(|(viewer, entries)| (*viewer, entries.iter().map(Watched::name).collect()))
            .collect();
        assert_eq!(
            shape,
            [
                (Some(Pk::new(2)), vec![Some("one"), None]),
                (Some(Pk::new(1)), vec![None, Some("two")]),
            ],
            "the entry with no viewer joins the account in use rather than a group of its own"
        );
    }
}
