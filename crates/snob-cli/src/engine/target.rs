//! Who the run is asking about.
//!
//! Two ways in, and they are not interchangeable. [`resolve`] asks Instagram,
//! which is the only way to learn about an account the tool has never seen and
//! the only way to know it is private. [`from_store`] asks the local database,
//! which costs nothing and is therefore the only one allowed when the network
//! is off.

use anyhow::Result;
use snob_core::Pk;
use snob_core::model::ListKind;
use snob_store::store::{Store, accounts, users};

use crate::app::{App, Viewer};
use crate::engine::ListQuery;

#[derive(Debug, Clone)]
pub struct Target {
    pub pk: Pk,
    /// As Instagram spells it, or as the store recorded it — never as it was
    /// typed. The upsert that follows writes this name back.
    ///
    /// `None` when it has genuinely never been learned, which happens on your
    /// own account whenever the session was stored without one: logging in
    /// while the account is throttled leaves it unset, and only `whoami` ever
    /// writes a resolved name back.
    ///
    /// Never `pk.to_string()` standing in for a name: `engine::decide` writes
    /// this straight into `users.username`, so the number would clobber a
    /// correct stored name, file a rename that never happened, and stop
    /// `find_pk_by_username` from finding the account.
    pub username: Option<String>,
    pub is_self: bool,
    /// The counters, when resolving already asked for them.
    ///
    /// Resolving a name and polling its counters are the **same request** to
    /// the same endpoint, so carrying the answer forward saves asking
    /// Instagram the identical question twice in a row — four times for a
    /// crossing.
    pub counters: Option<Counters>,
}

pub use snob_ig::model::Counters;

/// Strips the at sign people type out of habit. Both spellings mean the same
/// account, and Instagram takes neither with the sign attached.
pub fn clean(typed: &str) -> &str {
    typed.trim_start_matches('@')
}

/// How to name the account a run is about, before anything has resolved it.
///
/// The typed name when there is one, the viewer's own label otherwise. It goes
/// through `printable` because it is drawn on a terminal and `clean` only
/// strips the at sign.
pub fn label(app: &App, typed: Option<&str>) -> String {
    label_for(app.viewer(), typed)
}

/// [`label`] for a caller with no `App`, such as `--dry-run`.
pub fn label_for(viewer: &Viewer, typed: Option<&str>) -> String {
    match typed {
        Some(raw) => format!("@{}", snob_core::model::printable(clean(raw))),
        None => viewer.label(),
    }
}

/// The pk last seen under the name `typed`: the session's own when it is
/// the viewer's stored name, and otherwise from the local database. What
/// lets a profile be read by pk without first asking whose the name is.
/// Only a hint, which the profile's answer confirms (`profile_named`).
pub fn known_pk(app: &App, typed: &str) -> Result<Option<Pk>> {
    known_pk_in(app.viewer(), app.db(), typed)
}

/// [`known_pk`] for a caller with no `App`: the viewer and the database it
/// reads, which a report opens without writing (`--dry-run`).
pub fn known_pk_in(viewer: &Viewer, db: &Store, typed: &str) -> Result<Option<Pk>> {
    let name = clean(typed);
    if viewer
        .username
        .as_deref()
        .is_some_and(|own| own.eq_ignore_ascii_case(name))
    {
        return Ok(Some(viewer.pk));
    }
    Ok(users::pk_named(db.conn(), name)?)
}

/// [`known_pk`] for a read of `target`, or with none of the viewer's own
/// account, whose pk is the session's whatever name `typed` holds.
pub fn known_pk_of(app: &App, target: Option<&str>, typed: &str) -> Result<Option<Pk>> {
    match target {
        None => Ok(Some(app.viewer().pk)),
        Some(_) => known_pk(app, typed),
    }
}

/// Resolves against Instagram, spending one request when a name was given:
/// from the browser, one once the name's pk is known here and two the
/// first time.
///
/// A private account the viewer does not follow is refused **here**, before a
/// single page is walked: Instagram serves those lists to followers only, so
/// walking would buy nothing but empty pages.
pub async fn resolve(app: &mut App, args: &ListQuery) -> Result<Target> {
    let Some(typed) = args.target.as_deref() else {
        let viewer = app.viewer().clone();
        let resolved = match viewer.username {
            Some(u) => Some(u),
            None => app.client().resolve_username(viewer.pk).await?,
        };

        return Ok(Target {
            pk: viewer.pk,
            // `None` when it was never learned, rather than the numeric id
            // standing in for it. See the field.
            username: resolved.clone(),
            is_self: true,
            counters: match resolved {
                // Not asked for yet: your own account does not come through
                // the profile endpoint, and the poll reads its counters.
                Some(_) => None,
                // Without a real name, which only a run without a browser can
                // lack, there is nothing to poll **with** — the profile
                // endpoint takes a username — so saying "the counters are
                // unknown" costs nothing, while asking about a numeric id
                // would spend a request on a guaranteed 404 every single run.
                None => Some(Counters {
                    followers: None,
                    following: None,
                }),
            },
        });
    };

    let known = known_pk(app, typed)?;
    let profile = app.client().profile_named(clean(typed), known).await?;
    let is_self = app.viewer().pk == profile.id;

    // Only certainty blocks. This API has no contract, and a missing field must
    // never turn into a refusal: with `None` the walk runs and fails, or does
    // not, on its own terms.
    if !is_self && profile.is_private == Some(true) && profile.followed_by_viewer == Some(false) {
        // Which of the two refusals it is, and nothing about how either reads.
        // The name is filtered inside `report`, where it came off Instagram
        // rather than out of anybody's keyboard; `pfp.rs` refuses in the same
        // shape on the same field.
        return Err(crate::report::refuse_private(
            &profile.username,
            profile.requested_by_viewer == Some(true),
        ));
    }

    // The profile endpoint answers 400 for certain business accounts, and the
    // client falls back to search to get an id at all. Search has no counters,
    // so this run has none — and that is worth a sentence rather than a silent
    // `None`, because two things people expect quietly stop happening.
    //
    // `pager::verify_completion` compares a finished walk against the declared
    // size and skips the check entirely when there is nothing to compare with,
    // so the truncation wall — the one that catches Instagram serving 39 of
    // 21631 followers — cannot be detected on this account. And the counters
    // are what `--offline` weighs freshness against, so every run re-walks.
    //
    // Said here rather than in the client because this is where a *walk* is
    // being set up; `pfp` reaches the same fallback and loses nothing by it.
    if !profile.counters_are_knowable() {
        app.progress()
            .warn(&crate::report::counters_unknowable(&profile.username));
    }

    Ok(Target {
        is_self,
        pk: profile.id,
        // The same answer that named the account also counted it. Asking again
        // would be the identical request to the identical endpoint.
        //
        // `Some` with two `None`s inside it, never `None`: the outer one means
        // "nobody has asked", which would send `freshness::poll` off to ask
        // again and spend a request on the endpoint that just refused. The
        // inner ones mean "asked, and this route cannot say", which is the
        // truth. Neither is zero, and zero is the reading that would make every
        // short walk look complete.
        counters: Some(profile.counters()),
        username: Some(profile.username),
    })
}

/// Resolves from the local store alone, without touching the network.
///
/// A name that was never tracked has no snapshot either, so the refusal reads
/// the same as the cache miss that would have followed it.
pub fn from_store(app: &App, typed: Option<&str>, kind: ListKind) -> Result<Target> {
    let Some(typed) = typed else {
        let viewer = app.viewer();
        return Ok(Target {
            pk: viewer.pk,
            username: viewer.username.clone(),
            is_self: true,
            counters: None,
        });
    };

    let typed = clean(typed);
    let Some(pk) = accounts::find_pk_by_username(app.db().conn(), typed)? else {
        return Err(crate::report::refuse_nothing_stored(kind));
    };

    Ok(Target {
        pk,
        // Always a name: the account was found by the one that was typed, so the
        // worst case is the typed spelling rather than nothing. Said with `Some`
        // at the front, because the field means "never learned" and this path
        // cannot express that.
        username: Some(users::name(app.db().conn(), pk)?.unwrap_or_else(|| typed.to_string())),
        is_self: app.viewer().pk == pk,
        // Nothing was asked of Instagram, so there is nothing to carry.
        counters: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_at_sign_is_optional() {
        assert_eq!(clean("@someone"), "someone");
        assert_eq!(clean("someone"), "someone");
        // Only the leading one: it is not part of any username anyway.
        assert_eq!(clean("@@someone"), "someone");
    }
}
