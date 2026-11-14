//! What a finished child is told back to the client as.
//!
//! **Every string is filtered on the way out**, whatever field it is in. The
//! commands already filter the names they print, but a tool result is read by
//! a model, and the cost of a field that was missed is an instruction in
//! someone's bio arriving as text nobody on screen can see. `printable` is the
//! identity on everything a person can read, so filtering what was already
//! filtered changes nothing.
//!
//! **A list is handed back a page at a time** ([`page`]): a client reads a
//! tool's whole result into the model's context, and cuts what is too long
//! (Claude Code at 25,000 tokens, `MAX_MCP_OUTPUT_TOKENS`, as its documents
//! were quoted to us; not checked here). The page says the `count` of the
//! list the command printed and the `next_offset` to call again with, and
//! the later pages are handed back from what was already read (`pages.rs`,
//! `tools::Page`), with what the first page said of where the list came from.
//!
//! **A list is never handed back as more than it is.** One served from
//! storage says so and how old it is (`served`, [`not_read_now`]), and one
//! a walk cut short by `max_pages` says it is not the whole list
//! (`complete: false`, [`incomplete`]), since both are a success to the
//! command, whose warnings a person reads on standard error and an agent
//! never sees.
//!
//! **A picture is handed back as a picture** ([`take_pictures`]): a listing
//! asked for with its small pictures carries them in its JSON, in base64,
//! and they leave it here, to go out as image content beside the answer
//! (`mod.rs`, `saying`) rather than as text a model would read letter by
//! letter. A picture cannot be filtered as a string is: the descriptions
//! say that what one shows or says is data.

use serde_json::{Value, json};

use super::child::Finished;
use super::pages::{Picture, Whole};
use super::tools::Plan;
use crate::exit::ExitCode;

/// The result of a call: whether it failed, and the object to hand back.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub failed: bool,
    pub body: Value,
    /// The whole list, each item as its compact JSON, when `body` is one page
    /// of a longer one: for the server to hold for the pages after it, with
    /// what the page said about the list ([`ABOUT`]).
    pub whole: Option<Whole>,
    /// The small pictures that go with `body`, to hand back as images: all
    /// the call read, or for a page of a list, those of what the page holds.
    pub pictures: Vec<Picture>,
}

/// The fields of a list's answer that are about the whole list rather than
/// the page, which every later page handed back from it repeats: where it
/// came from and when, when a cooldown lifts, and whether it is all of it.
const ABOUT: [&str; 3] = ["served", "cooldown_until", "complete"];

/// The most accounts on a page.
pub const PAGE: usize = 200;

/// The most bytes of items on a page, as compact JSON, before the result
/// is filtered. An account is 100 to 150 bytes once its picture's address
/// is left out ([`LEFT_OUT`]), so a page is [`PAGE`] accounts; the bound is
/// for longer ones, names in scripts that take several bytes a character.
/// The result is sent twice, as the structured content and as its text (the
/// specification asks for both), so 30,000 bytes keeps a page near 20,000
/// tokens in all, under the cut quoted above.
pub const PAGE_BYTES: usize = 30_000;

/// What an agent is told when there is no session to act with.
///
/// **It replaces the command's own hint** (`run "snob login"`), which is
/// advice to the person at the terminal: read by a model, it is an invitation
/// to run `login` itself, which no tool offers and which needs the password
/// or a browser window only the person has. The server's instructions
/// (`mod.rs`, `get_info`) say the same, so a model that skipped one still
/// reads the other.
pub const SIGN_IN: &str = "the person signs in with \"snob login\" in their own terminal; \
                           never ask them for a password or a session id";

/// What an agent is told to do about a refusal, by the reason the command
/// gave it, **in place of the command's own hint**, which is advice to the
/// person at a terminal: a command to type (`snob jobs cancel`), or a flag
/// the agent never passed and cannot (`--offline`, which its server adds for
/// a stored list, `--max-wait`, which its server sets). As [`SIGN_IN`] is
/// for no session; a reason not named here keeps its hint, which is already
/// one an agent can act on.
fn agent_hint(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "too_many_jobs" => {
            "one job runs at a time for the account: ask snob_job how that one is going (its \
             id is in the message), or stop it with snob_job_cancel, and call this again once \
             it has ended"
        }
        "no_such_job" => "snob_jobs lists this account's jobs and their ids",
        "no_such_snapshot" => {
            "snob_snapshots lists the captures stored for the account and their ids; only \
             one whose walk completed can be compared"
        }
        "job_stopping" => {
            "the account's job was asked to stop and is stopping: snob_job tells when it has \
             stopped (its id is in the message); call this again then"
        }
        "nothing_stored" => {
            "nothing of this list is stored yet: call snob_list again without offset and with \
             mode \"auto\" to read it"
        }
        "stored_moments" => {
            "the two stored lists were read too far apart to be crossed: call snob_list again \
             without offset and with mode \"auto\" (not \"stored\"), so both lists are checked \
             against the account"
        }
        "busy" => {
            "another call that reaches Instagram is running for this account, and one runs \
             at a time; nothing was sent: call this again once it has ended, not beside it \
             (snob_jobs names a job that is walking, and snob_job_cancel stops it)"
        }
        "mcp_write_cap" => {
            "agents have made as many follows and unfollows on this account as it allows \
             them in a day: do not call this again before retry_at (seconds since the \
             epoch); tell the person, who can make it in their own terminal"
        }
        "walked_elsewhere" => {
            "another walk holds this list and nothing was started: when retry_at is set \
             (seconds since the epoch), a job that stopped without letting go holds it until \
             then, so call this again only after it; otherwise call it again once that walk \
             has finished, and it uses what the walk stored"
        }
        "budget_wait" => {
            "the request budget has no room within the wait this server allows: call it again \
             later; snob_status says when the budget has room"
        }
        _ => return None,
    })
}

/// What an agent is told of a push-back met by this very call, which
/// started a cooldown the failure does not date (`report::error_json_for`
/// dates only one that refused the run): the command's own sentence says to
/// run it again once the cooldown lifts, which an agent cannot see.
pub const PUSHED_BACK: &str = "Instagram pushed back and a cooldown has started for this \
                               account: snob_status, which sends nothing, says when it lifts; \
                               call nothing that reaches Instagram before then, and tell the \
                               person when the wait ends";

/// Reads a finished child against what its plan counts as an answer.
pub fn from(plan: &Plan, mut finished: Finished) -> Answer {
    // First, so that no picture's bytes are walked as text by what follows.
    let pictures = finished
        .json
        .as_mut()
        .map(take_pictures)
        .unwrap_or_default();
    if shows_media(plan) {
        finished.json = finished.json.take().map(without_media_addresses);
    }
    if is_changes(plan) {
        finished.json = finished.json.take().map(bounded_changes);
    }
    let exit = finished.code;
    let offset = plan.page.as_ref().map_or(0, |page| page.offset);
    let limit = plan.page.as_ref().and_then(|page| page.limit);
    let mut whole_items = None;
    // What a document that holds the list says beside it (`Page::rows`),
    // which every later page repeats.
    let mut beside: Vec<String> = Vec::new();
    // Past 0, only with the document it is an answer for: `status` and a dry
    // run print one whatever their exit, and the same exit with nothing on
    // standard output is a refusal, which an empty answer would hide.
    let answered = plan.answers.contains(&exit)
        && (exit == ExitCode::Ok || matches!(finished.json, Some(Value::Object(_))));
    let mut body = if answered {
        // A structured result is an object, so whatever else a command printed
        // goes under a key. **Never index into what came back before it is
        // known to be an object**: `serde_json` panics on `value["key"] = ...`
        // for a string or a number, and under `panic = "abort"` that is the
        // whole server gone, every call in flight with it, over one line a
        // command printed.
        match finished.json {
            Some(Value::Array(items)) => {
                let items = compact(&items);
                if let Some(refused) = past_the_end(items.len(), offset, limit) {
                    return refused;
                }
                // A call that takes no offset has no next page to offer.
                let body = page(&items, offset, limit, plan.page.is_some());
                if !body["next_offset"].is_null() {
                    whole_items = Some(items);
                }
                body
            }
            Some(Value::Object(mut document)) => {
                let rows = plan.page.as_ref().and_then(|page| page.rows);
                match rows.and_then(|key| document.remove(key).map(|rows| (key, rows))) {
                    Some((_, Value::Array(items))) => {
                        let items = compact(&items);
                        if let Some(refused) = past_the_end(items.len(), offset, limit) {
                            return refused;
                        }
                        let brief = plan.page.as_ref().is_some_and(|page| page.brief);
                        let mut body = page(&shaped(&items, brief), offset, limit, true);
                        if !body["next_offset"].is_null() {
                            whole_items = Some(items);
                        }
                        if let Some(fields) = body.as_object_mut() {
                            for (key, value) in document {
                                if !fields.contains_key(&key) {
                                    beside.push(key.clone());
                                    fields.insert(key, value);
                                }
                            }
                        }
                        body
                    }
                    // Not a list where one was expected (`"posts": null` for
                    // an account with none): the document as it came.
                    Some((key, other)) => {
                        document.insert(key.to_string(), other);
                        Value::Object(document)
                    }
                    None => {
                        let mut document = Value::Object(document);
                        leave_out_of_accounts(&mut document);
                        document
                    }
                }
            }
            Some(other) => json!({ "value": other }),
            None => json!({ "text": finished.text.trim() }),
        }
    } else {
        let said = || {
            json!({
                "code": exit.as_str(),
                "message": last_lines(&finished.said),
            })
        };
        // `main::wording_for` prints an object under `error`, but the line is
        // only known to be JSON with that key: `{"error":"x"}` is read here
        // too, and its `error` is put in an object rather than indexed into.
        let mut failure = match finished.error {
            Some(Value::Object(mut error)) => match error.remove("error") {
                Some(inner @ Value::Object(_)) => inner,
                Some(Value::Null) => said(),
                Some(other) => json!({ "code": exit.as_str(), "message": other }),
                None => Value::Object(error),
            },
            _ => said(),
        };
        // A finer reason the command gave (`busy`, from `--spend-alone`) is
        // the code an agent tells this failure apart by, the same one the
        // server's own refusal of a busy call has; `exit` keeps the exit's
        // token. `failure` is an object on every branch above.
        if let Some(reason) = failure
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if let Some(hint) = agent_hint(&reason) {
                failure["hint"] = json!(hint);
            }
            failure["code"] = json!(reason);
        }
        // What a list walk kept is still worth having: it is stored, and the
        // next call continues from it. One page of it, with no next one:
        // what a walk stopped short read is not stored as a whole list, so a
        // later page could not be read back, and the next call for the list
        // continues the walk instead.
        if let Some(partial) = finished.json {
            failure["partial"] = match partial {
                Value::Array(items) => page(&compact(&items), offset, limit, false),
                other => other,
            };
        }
        if exit == ExitCode::NoSession {
            failure["hint"] = json!(SIGN_IN);
        }
        // A push-back this call met: the cooldown it started is not in the
        // failure, and the command's advice is to type it again later.
        if exit == ExitCode::RateLimited
            && !failure.get("cooldown_until").is_some_and(Value::is_i64)
        {
            failure["hint"] = json!(PUSHED_BACK);
        }
        return Answer {
            failed: true,
            body: cleaned(json!({ "exit": exit.as_str(), "error": failure })),
            whole: None,
            pictures: Vec::new(),
        };
    };
    // Where the list came from, as its walk said it (`Progress::served`), and
    // when a cooldown that had it served from storage lifts: a stored list
    // is handed to the agent as one, with its date, as a person is told it
    // in the warning, never as if it had just been read. `body` is an object
    // for every list (a page), and nothing is indexed into one that is not.
    //
    // **And whether it is all of the list**: a walk cut short by `max_pages`
    // is a success, exit 0, and its `reason` (`page_limit`), with how many
    // accounts it read of how many the counter declared, is the only place
    // that says the list is short. An agent told nothing would give `count`
    // as how many followers the account has.
    if !finished.served.is_empty() && body.is_object() {
        let served: Vec<Value> = finished
            .served
            .iter()
            .map(|said| {
                let mut told = json!({
                    "list": said.get("list"),
                    "from": said.get("from"),
                    "taken_at": said.get("taken_at"),
                });
                if let Some(own) = said.get("own_changes").filter(|n| n.is_u64()) {
                    told["own_changes"] = own.clone();
                }
                if let Some(reason) = said
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|reason| *reason != "completed")
                {
                    told["stopped"] = json!(reason);
                    told["read"] = said.get("accounts").cloned().unwrap_or(Value::Null);
                    told["of"] = said.get("of").cloned().unwrap_or(Value::Null);
                }
                told
            })
            .collect();
        if served.iter().any(|told| told.get("stopped").is_some()) {
            body["complete"] = json!(false);
        }
        let lifts = finished
            .served
            .iter()
            .filter_map(|said| said.get("cooldown_until"))
            .find(|until| until.is_i64());
        if let Some(lifts) = lifts {
            body["cooldown_until"] = lifts.clone();
        }
        body["served"] = Value::Array(served);
    }
    if exit != ExitCode::Ok {
        body["exit"] = json!(exit.as_str());
    }
    if exit == ExitCode::NoSession {
        // `status` puts its error object beside the sections it could not
        // fill; the hint goes in it when it is an object, and beside it
        // otherwise, never by indexing into what is not known to be one.
        match body.get_mut("error").and_then(Value::as_object_mut) {
            Some(error) => {
                error.insert("hint".into(), json!(SIGN_IN));
            }
            None => body["hint"] = json!(SIGN_IN),
        }
    }
    let shown = on_this_page(&mut body, &pictures);
    let whole = whole_items.map(|items| Whole {
        items,
        about: ABOUT
            .iter()
            .copied()
            .chain(beside.iter().map(String::as_str))
            .filter_map(|key| Some((key.to_string(), body.get(key)?.clone())))
            .collect(),
        pictures,
    });
    Answer {
        failed: false,
        body: cleaned(body),
        whole,
        pictures: shown,
    }
}

/// The kinds of picture handed on as images: what `media::small_pictures`
/// says a picture is, and nothing a child could make up besides.
const PICTURES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];

/// The most one picture may weigh in base64 to be handed on: what
/// `media::MAX_SMALL_PICTURE` comes to, and a third over.
const MOST_PICTURE: usize = 700 * 1024;

/// Takes the small pictures out of the document a child printed
/// (`media::small_pictures`, under `pictures`): each as what the answer
/// says of it, and its bytes when it has them, **which are no longer in
/// the document**, so nothing after this reads a picture as text.
///
/// Read without assuming its shape, for the reason [`from`] gives: an entry
/// that is no object or has no number is dropped, and bytes that are not
/// base64, weigh too much or come under a kind that is no picture are
/// said as missing, never handed on.
pub fn take_pictures(document: &mut Value) -> Vec<Picture> {
    let Some(Value::Array(entries)) = document
        .as_object_mut()
        .and_then(|fields| fields.remove("pictures"))
    else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let Value::Object(mut entry) = entry else {
                return None;
            };
            let of = entry.get("of")?.as_u64()?;
            let data = match entry.remove("data") {
                Some(Value::String(data)) => Some(data),
                _ => None,
            };
            let mime = entry
                .get("mime")
                .and_then(Value::as_str)
                .filter(|mime| PICTURES.contains(mime))
                .map(str::to_string);
            let sound = |data: &String| {
                data.len() <= MOST_PICTURE
                    && !data.is_empty()
                    && data
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
            };
            let image = match (mime, data) {
                (Some(mime), Some(data)) if sound(&data) => Some((mime, data)),
                _ => None,
            };
            let said = match &image {
                Some(_) => json!({
                    "of": of,
                    "mime": entry.get("mime"),
                    "bytes": entry.get("bytes"),
                }),
                None => json!({
                    "of": of,
                    "missing": entry
                        .get("missing")
                        .and_then(Value::as_str)
                        .unwrap_or("it could not be read"),
                }),
            };
            Some(Picture { of, said, image })
        })
        .collect()
}

/// The pictures of `pictures` that go with `body`, which is told of them
/// under `pictures`: for a page of a list (`offset` and `items`), those of
/// what the page holds, numbered from one as the listing numbers them, and
/// for any other answer all of them.
fn on_this_page(body: &mut Value, pictures: &[Picture]) -> Vec<Picture> {
    if pictures.is_empty() {
        return Vec::new();
    }
    let page = body
        .get("offset")
        .and_then(Value::as_u64)
        .zip(body.get("items").and_then(Value::as_array).map(Vec::len));
    let shown: Vec<Picture> = pictures
        .iter()
        .filter(|picture| match page {
            Some((offset, items)) => {
                picture.of > offset && picture.of <= offset.saturating_add(items as u64)
            }
            None => true,
        })
        .cloned()
        .collect();
    if let Some(fields) = body.as_object_mut() {
        fields.insert(
            "pictures".into(),
            Value::Array(shown.iter().map(|picture| picture.said.clone()).collect()),
        );
    }
    shown
}

/// The page at `offset` of a list the server holds (`pages.rs`), as the
/// answer of a call that ran nothing, saying what the first page said of
/// the whole list.
///
/// `brief`: each item as [`briefly`] leaves it. The list is held whole, so a
/// page may be asked for either way whichever way the first one was.
pub fn held(whole: &Whole, offset: usize, limit: Option<usize>, brief: bool) -> Answer {
    if let Some(refused) = past_the_end(whole.items.len(), offset, limit) {
        return refused;
    }
    let mut body = page(&shaped(&whole.items, brief), offset, limit, true);
    if let Some(fields) = body.as_object_mut() {
        fields.extend(whole.about.clone());
    }
    let shown = on_this_page(&mut body, &whole.pictures);
    Answer {
        failed: false,
        body: cleaned(body),
        whole: None,
        pictures: shown,
    }
}

/// The refusal of an `offset` at or past the end of what the call hands
/// back, `offset_past_end`, with `count` and `total` beside it.
///
/// **Refused, not an empty page.** An empty page beside `count: 5` read as a
/// contradiction, and an empty page at all read as "no more accounts" when
/// the likelier mistake was taking `limit` for the size of one page: it caps
/// the whole answer. An agent that follows `next_offset` never gets here,
/// since the last page has none. An offset of 0 is a page however short
/// the list, an empty one included.
fn past_the_end(total: usize, offset: usize, limit: Option<usize>) -> Option<Answer> {
    let count = limit.map_or(total, |limit| limit.min(total));
    if offset == 0 || offset < count {
        return None;
    }
    let capped = if count < total {
        format!(
            " (limit caps the whole answer, not one page: the list has {total}; call with a \
             higher limit, or none, for the rest)"
        )
    } else {
        String::new()
    };
    let message = format!(
        "offset {offset} is past the end: this call hands back {count} accounts in all, at \
         offsets 0 to {last}{capped}. Follow next_offset for the next page; the last page \
         has none.",
        last = count.saturating_sub(1),
    );
    Some(Answer {
        failed: true,
        // Shaped as the server's other refusals of an argument (`mod.rs`,
        // `refusal`): nothing ran, so there is no exit to report.
        body: cleaned(json!({
            "error": {
                "code": "offset_past_end",
                "message": message,
                "count": count,
                "total": total,
            },
        })),
        whole: None,
        pictures: Vec::new(),
    })
}

/// What an agent is not handed of an account: its picture's address, a
/// signed URL of 300 to 500 bytes that answers nothing an agent is asked
/// and would make most of a page.
const LEFT_OUT: [&str; 1] = ["pfp_url"];

/// [`LEFT_OUT`] taken out of the accounts a document names, as a list's
/// are: the mutual followers of a profile (`followed_by.accounts`), whose
/// pictures' addresses were most of `snob_profile`'s answer. Looked up with
/// `get_mut`, never indexed, for the reason [`from`] gives.
fn leave_out_of_accounts(document: &mut Value) {
    let accounts = document
        .get_mut("followed_by")
        .and_then(|followed| followed.get_mut("accounts"))
        .and_then(Value::as_array_mut);
    for account in accounts.into_iter().flatten() {
        if let Some(fields) = account.as_object_mut() {
            for key in LEFT_OUT {
                fields.remove(key);
            }
        }
    }
}

/// Whether `plan` is `snob_changes`.
fn is_changes(plan: &Plan) -> bool {
    plan.args.first().map(String::as_str) == Some("changes")
}

/// What an agent is told when [`bounded_changes`] left accounts out.
const CHANGES_CUT: &str = "More accounts changed than fit one answer: each list of accounts \
                           is cut, and gained_count, lost_count and renamed_count say how \
                           many there are in all. Ask for one list, or for two captures \
                           closer together (from and to, by the ids snob_snapshots lists)";

/// The document `changes` printed, as `snob_changes` hands it back: every
/// account without the fields [`LEFT_OUT`], as a list's accounts go, and
/// the accounts of all its lists together within [`PAGE_BYTES`].
///
/// **Two captures far apart differ by thousands of accounts**, each printed
/// with its picture's signed address, and nothing pages this answer: it
/// was megabytes of query strings in a model's context. What does not fit
/// is left out, in the order the document has it; `gained_count`,
/// `lost_count` and `renamed_count` are how many there are whatever was
/// kept, and `cut` says so in a sentence. Nothing is indexed, for the
/// reason [`from`] gives.
fn bounded_changes(mut document: Value) -> Value {
    let mut left = PAGE_BYTES;
    let mut cut = false;
    let lists = document.get_mut("lists").and_then(Value::as_array_mut);
    for list in lists.into_iter().flatten() {
        let Some(list) = list.as_object_mut() else {
            continue;
        };
        for key in ["gained", "lost", "renamed"] {
            let Some(rows) = list.get_mut(key).and_then(Value::as_array_mut) else {
                continue;
            };
            let total = rows.len();
            let mut kept = 0;
            for row in rows.iter_mut() {
                if let Some(fields) = row.as_object_mut() {
                    for field in LEFT_OUT {
                        fields.remove(field);
                    }
                }
                let size = row.to_string().len();
                if size > left {
                    break;
                }
                left -= size;
                kept += 1;
            }
            rows.truncate(kept);
            cut |= kept < total;
            list.insert(format!("{key}_count"), json!(total));
        }
    }
    if cut && let Some(fields) = document.as_object_mut() {
        fields.insert("cut".to_string(), json!(CHANGES_CUT));
    }
    document
}

/// Each item as its compact JSON, which is what a page is measured in and
/// what the server holds, without the fields [`LEFT_OUT`].
fn compact(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            Value::Object(fields) => {
                let mut fields = fields.clone();
                for key in LEFT_OUT {
                    fields.remove(key);
                }
                Value::Object(fields).to_string()
            }
            other => other.to_string(),
        })
        .collect()
}

/// What `brief` keeps of a post: what tells it from every other, where it
/// is, and what orders it. Everything an agent needs to say which posts it
/// does not have yet, and a tenth of what a post's answer weighs.
const BRIEF: [&str; 7] = ["pk", "code", "url", "owner", "kind", "taken_at", "pinned"];

/// `items` as a page hands them back: as they are, or each as [`briefly`]
/// leaves it.
fn shaped(items: &[String], brief: bool) -> std::borrow::Cow<'_, [String]> {
    if brief {
        items.iter().map(|item| briefly(item)).collect()
    } else {
        items.into()
    }
}

/// One item, as its compact JSON, with only the keys of [`BRIEF`] and how
/// many items it holds (`item_count`). One that is no object stays as it
/// is. Read with `get`, never indexed, for the reason [`from`] gives.
fn briefly(item: &str) -> String {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(item) else {
        return item.to_string();
    };
    let mut kept: serde_json::Map<String, Value> = BRIEF
        .iter()
        .filter_map(|key| Some(((*key).to_string(), fields.get(*key)?.clone())))
        .collect();
    if let Some(items) = fields.get("items").and_then(Value::as_array) {
        kept.insert("item_count".into(), json!(items.len()));
    }
    Value::Object(kept).to_string()
}

/// The page of `items` that starts at `offset`: as many as fit in
/// [`PAGE_BYTES`], at most [`PAGE`] and at least one, so a page always moves
/// on. `{total, count, offset, items, next_offset}`, where `total` is how
/// many the command printed (the whole list, or what a walk cut short read,
/// which `complete: false` says), `count` how many of them the call hands
/// back across its pages (`total`, or `limit` when that is fewer), and
/// `next_offset` is null on the last page, or always when the call cannot
/// be made again for another (`pages` false). An offset past the end is
/// refused before this ([`past_the_end`]) on a success, and an empty page
/// for what a failed walk kept. **Sliced with `get`, never by indexing**:
/// the offset is the client's number, and a panic here is the whole server
/// gone.
pub fn page(items: &[String], offset: usize, limit: Option<usize>, pages: bool) -> Value {
    let total = items.len();
    let count = limit.map_or(total, |limit| limit.min(total));
    let items = items.get(..count).unwrap_or_default();
    let rest = items.get(offset.min(count)..).unwrap_or_default();
    let (mut taken, mut bytes) = (0, 0);
    for item in rest.iter().take(PAGE) {
        bytes += item.len();
        if taken > 0 && bytes > PAGE_BYTES {
            break;
        }
        taken += 1;
    }
    let shown: Vec<Value> = rest
        .get(..taken)
        .unwrap_or_default()
        .iter()
        // Read back from what `compact` wrote, so it does not fail; a null
        // would still be better than a panic.
        .map(|item| serde_json::from_str(item).unwrap_or(Value::Null))
        .collect();
    let end = offset.min(count) + taken;
    let next_offset = (pages && end < count).then_some(end);
    json!({
        "total": total,
        "count": count,
        "offset": offset,
        "items": shown,
        "next_offset": next_offset,
    })
}

/// What an agent is told of a page with another after it, beside the JSON:
/// which accounts these are, and how the next ones are had, by calling
/// `tool` again (`snob_list`, or `snob_job` for a job's result). `None` for
/// any other answer.
pub fn next_page(body: &Value, tool: &str) -> Option<String> {
    let next = body.get("next_offset")?.as_u64()?;
    let count = body.get("count")?.as_u64()?;
    let offset = body.get("offset")?.as_u64()?;
    let of = match body.get("total").and_then(Value::as_u64) {
        Some(total) if total > count => format!("{count} (the limit; the list has {total})"),
        _ => count.to_string(),
    };
    // A confirmation on a call that asks nothing is refused, and only a
    // list's call may carry one.
    let and = if tool == "snob_list" {
        ", no confirmation,"
    } else {
        ""
    };
    // What a page is of: a grid's are posts.
    let what = match tool {
        "snob_posts" => "Posts",
        "snob_snapshots" => "Captures",
        _ => "Accounts",
    };
    Some(format!(
        "{what} {first} to {next} of {of}. For the next ones, call {tool} again with \
         the same arguments{and} and \"offset\": {next}; a later page is handed back from \
         what was already read, and sends nothing and asks nothing.",
        first = offset + 1,
    ))
}

/// What an agent is told beside a list that was not read now, **however
/// old it is**: served from storage in a cooldown, or because the check of
/// its counter failed (`engine::cooldown::serve`, `engine::freshness`), or
/// because the call asked for what is stored (`mode: "stored"`, and a later
/// page read from the store), or because its counter moved but it was
/// walked too recently to walk again. A list whose counter was checked now
/// and had not moved was as good as read now, and has no sentence. The
/// moments in UTC: the agent and the person may be in another zone than
/// this machine. `None` for any other answer.
pub fn not_read_now(body: &Value) -> Option<String> {
    let served = body.get("served")?.as_array()?;
    let dated = |wanted: &[&str]| -> Vec<String> {
        served
            .iter()
            .filter(|said| {
                said.get("from")
                    .and_then(Value::as_str)
                    .is_some_and(|from| wanted.contains(&from))
            })
            .filter_map(|said| {
                let list = said.get("list")?.as_str()?;
                let at = said.get("taken_at")?.as_i64()?;
                Some(format!("the {list} list as it was stored at {}", utc(at)))
            })
            .collect()
    };
    let mut said = Vec::new();
    let stale = dated(&["stored_in_cooldown", "stored_after_failed_poll"]);
    if !stale.is_empty() {
        let why = match body.get("cooldown_until").and_then(Value::as_i64) {
            Some(until) => format!(
                "the account is in cooldown until {}, when nothing may be sent",
                utc(until)
            ),
            None => "checking whether it had changed failed".to_string(),
        };
        said.push(format!(
            "Not read now: {why}, so this is {}",
            stale.join(" and ")
        ));
    }
    let asked = dated(&["stored"]);
    if !asked.is_empty() {
        said.push(format!(
            "Read from what is stored, not from Instagram: this is {}",
            asked.join(" and ")
        ));
    }
    let recent = dated(&["stored_walked_recently"]);
    if !recent.is_empty() {
        said.push(format!(
            "Not read now: its counter has moved, but it was walked too recently to walk \
             again, so this is {}",
            recent.join(" and ")
        ));
    }
    let own: u64 = served
        .iter()
        .filter_map(|said| said.get("own_changes")?.as_u64())
        .sum();
    if said.is_empty() {
        return (own > 0).then(|| own_changes(own));
    }
    let mut told = format!(
        "{}, however old that is. Say so, with that date, when you present it.",
        said.join("; ")
    );
    if own > 0 {
        told.push(' ');
        told.push_str(&own_changes(own));
    }
    Some(told)
}

/// The sentence for a stored following list handed out with the account's
/// own follows and unfollows since its walk applied (`own_changes`).
fn own_changes(n: u64) -> String {
    let changes = if n == 1 { "change" } else { "changes" };
    format!(
        "The following list includes {n} {changes} the person made through snob since it \
         was read (own_changes); those are known, having been sent, but the list was not \
         read again."
    )
}

/// What an agent is told beside a list its `limit` cut, which then has no
/// next page: that the list has more, so `count` is not read as its size.
/// A page with another after it says so in [`next_page`] instead.
pub fn limited(body: &Value) -> Option<String> {
    if !body.get("next_offset")?.is_null() {
        return None;
    }
    let count = body.get("count")?.as_u64()?;
    let total = body.get("total")?.as_u64()?;
    (total > count).then(|| {
        format!(
            "These are the first {count} accounts, as limit asked: the list has {total}. \
             Call again without limit, or with a higher one, for the rest."
        )
    })
}

/// What an agent is told beside a list that is not all of it: a walk cut
/// short by `max_pages`, or stopped for another reason that still printed
/// what it read. `None` for a whole list.
///
/// **A crossing is told otherwise**: its `count` is what it found among the
/// accounts read of a list cut short, against the whole of the other (a cut
/// list to cross against is refused, `SetOp::require_against_complete`), so
/// some results are missing and the ones given are right, as a person is
/// told it (`commands::sets`). Its body is the one that says where two
/// lists came from.
pub fn incomplete(body: &Value) -> Option<String> {
    if body.get("complete").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let served = body.get("served")?.as_array()?;
    let crossing = served.len() > 1;
    let short: Vec<String> = served
        .iter()
        .filter_map(|said| {
            let list = said.get("list")?.as_str()?;
            let why = match said.get("stopped")?.as_str()? {
                "page_limit" => "at max_pages".to_string(),
                "truncated" => "where Instagram stopped serving pages".to_string(),
                other => format!("early ({other})"),
            };
            let read = said.get("read").and_then(Value::as_u64).unwrap_or(0);
            let of = match said.get("of").and_then(Value::as_u64) {
                Some(of) => format!(" of about {of}"),
                None => String::new(),
            };
            Some(format!(
                "the {list} list stopped {why}, with {read}{of} accounts read"
            ))
        })
        .collect();
    if short.is_empty() {
        return None;
    }
    let count = if crossing {
        "some results are missing and the ones given are right: count is how many were \
         found among the accounts read, not how many there are"
    } else {
        "count is what was read, not how many the account has"
    };
    Some(format!(
        "Not the whole list: {}, so {count}; say so when you present it. Calling snob_list \
         again without max_pages reads the rest.",
        short.join(" and ")
    ))
}

/// A moment in seconds since the epoch, in UTC, as an agent is told it.
fn utc(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || seconds.to_string(),
        |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// The last few lines of what a command said, which is where its sentence
/// about the failure is: the records of the log left out first
/// (`jobs::show::sentences`), which carry what Instagram answered and would
/// push the sentence out, and the lines of `--progress-json`, which a stored
/// read is given for its `served` line.
fn last_lines(said: &str) -> String {
    let lines = crate::jobs::show::sentences(
        said.lines()
            .filter(|line| !line.trim().is_empty())
            .filter(|line| {
                serde_json::from_str::<Value>(line.trim())
                    .ok()
                    .is_none_or(|value| value.get("progress").is_none())
            })
            .collect(),
    );
    lines[lines.len().saturating_sub(6)..].join("\n")
}

/// Whether `plan` is one of the commands about posts, stories or
/// highlights (`snob_posts`, `snob_post`, `snob_post_likers`,
/// `snob_post_download`, `snob_stories`, `snob_highlights`), whose answers
/// lose their media addresses ([`without_media_addresses`]).
fn shows_media(plan: &Plan) -> bool {
    matches!(
        plan.args.first().map(String::as_str),
        Some("posts" | "post" | "likers" | "stories" | "highlights")
    )
}

/// `value` with every address that is not a page of instagram.com made
/// null, wherever it is: the pictures and videos a post's answer names, its
/// best tracks', and the profile pictures of the accounts in it.
///
/// **An address is a download under another name**, which is the server's
/// to make, into its own directory and within its bounds (AGENTS.md, the
/// MCP server), so no agent is handed one, and each is a signed address of
/// a few hundred bytes that tokenizes at about three bytes a token: a page
/// of posts would be mostly query strings. Made null rather than removed, so every key the command
/// prints is still there. Judged by host, so a test's own server counts as a
/// CDN as Instagram's does, and the post's own link stays.
pub fn without_media_addresses(value: Value) -> Value {
    match value {
        Value::String(text) if is_a_media_address(&text) => Value::Null,
        Value::Array(items) => {
            Value::Array(items.into_iter().map(without_media_addresses).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, without_media_addresses(value)))
                .collect(),
        ),
        other => other,
    }
}

/// An `http` or `https` address anywhere but instagram.com.
fn is_a_media_address(text: &str) -> bool {
    let Ok(address) = url::Url::parse(text) else {
        return false;
    };
    matches!(address.scheme(), "http" | "https")
        && !address
            .host_str()
            .is_some_and(|host| host == "instagram.com" || host.ends_with(".instagram.com"))
}

/// `value` with every string, keys included, through `printable`.
pub fn cleaned(value: Value) -> Value {
    use snob_core::model::printable;
    match value {
        Value::String(text) => Value::String(printable(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(cleaned).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (printable(&key), cleaned(value)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(answers: &'static [ExitCode]) -> Plan {
        Plan {
            args: vec![],
            ask: None,
            answers,
            spends: crate::commands::mcp::tools::Spends::Nothing,
            page: None,
            run: crate::commands::mcp::tools::Run::Inline,
            acting_as: None,
            records: true,
        }
    }

    /// The grid's plan, whose list is the `posts` of its document.
    fn grid_at(offset: usize) -> Plan {
        Plan {
            args: vec!["posts".into()],
            page: Some(crate::commands::mcp::tools::Page {
                offset,
                of: "the grid".into(),
                held_only: true,
                limit: None,
                rows: Some("posts"),
                brief: false,
            }),
            ..plan(&[ExitCode::Ok])
        }
    }

    /// A listing's small pictures leave its JSON as images, each with the
    /// page that holds what it is of; what is no sound picture is said as
    /// missing and never handed on.
    #[test]
    fn pictures_leave_the_json_and_go_with_their_page() {
        let caption = "x".repeat(4_000);
        let posts: Vec<Value> = (1..=20)
            .map(|n| json!({ "number": n, "caption": caption }))
            .collect();
        let mut pictures: Vec<Value> = (1..=20)
            .map(|n| json!({ "of": n, "mime": "image/jpeg", "bytes": 3, "data": "QUJD" }))
            .collect();
        pictures[1] = json!({ "of": 2, "missing": "none is offered" });
        pictures[2] = json!({ "of": 3, "mime": "text/html", "bytes": 3, "data": "QUJD" });
        pictures[3] = json!({ "of": 4, "mime": "image/png", "bytes": 3, "data": "<script>" });
        pictures.push(json!("not an entry"));
        pictures.push(json!({ "mime": "image/png", "data": "QUJD" }));
        let told = from(
            &grid_at(0),
            finished(
                ExitCode::Ok,
                Some(json!({ "username": "someone", "posts": posts, "pictures": pictures })),
                None,
            ),
        );
        let next = told.body["next_offset"].as_u64().expect("another page") as usize;
        assert!((4..20).contains(&next), "{next}");
        // The first page's pictures and no other's, the bytes out of the body.
        let shown: Vec<u64> = told.pictures.iter().map(|picture| picture.of).collect();
        assert_eq!(shown, (1..=next as u64).collect::<Vec<_>>());
        assert_eq!(
            told.pictures[0].image,
            Some(("image/jpeg".to_string(), "QUJD".to_string()))
        );
        for unsound in &told.pictures[1..=3] {
            assert_eq!(unsound.image, None, "{}", unsound.of);
        }
        assert_eq!(
            told.body["pictures"][0],
            json!({ "of": 1, "mime": "image/jpeg", "bytes": 3 })
        );
        assert_eq!(told.body["pictures"][1]["missing"], "none is offered");
        assert_eq!(told.body["pictures"][2]["missing"], "it could not be read");
        assert!(!told.body.to_string().contains("QUJD"));
        assert_eq!(
            told.body["pictures"].as_array().map(Vec::len),
            Some(next),
            "the page's alone"
        );

        // The next page, from what is held, has its own.
        let whole = told.whole.expect("held with its pictures");
        assert_eq!(whole.pictures.len(), 20);
        let later = held(&whole, next, None, false);
        assert_eq!(later.pictures.first().map(|p| p.of), Some(next as u64 + 1));
        assert_eq!(later.body["pictures"][0]["of"], next as u64 + 1);

        // A document that is no list hands back every picture.
        let post = from(
            &Plan {
                args: vec!["post".into()],
                ..plan(&[ExitCode::Ok])
            },
            finished(
                ExitCode::Ok,
                Some(json!({
                    "post": { "items": [{ "number": 1 }, { "number": 2 }] },
                    "pictures": [
                        { "of": 1, "mime": "image/webp", "bytes": 3, "data": "QUJD" },
                        { "of": 2, "missing": "none is offered" },
                    ],
                })),
                None,
            ),
        );
        assert_eq!(post.pictures.len(), 2);
        assert!(post.pictures[0].image.is_some());
        assert_eq!(post.body["pictures"][1]["missing"], "none is offered");
        // And one that printed none says nothing of pictures.
        let plain = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Ok, Some(json!({ "ok": true })), None),
        );
        assert!(plain.pictures.is_empty());
        assert!(plain.body.get("pictures").is_none());
    }

    /// A brief page holds what tells its posts apart and nothing else, and
    /// the list is held whole all the same: the next page may be asked for
    /// in full.
    #[test]
    fn a_brief_page_keeps_what_tells_posts_apart() {
        let caption = "x".repeat(4_000);
        let posts: Vec<Value> = (0..20)
            .map(|n| {
                json!({
                    "pk": n.to_string(), "code": format!("code{n}"), "taken_at": 1_790_000_000 - n,
                    "url": format!("https://www.instagram.com/p/code{n}/"), "owner": "someone",
                    "kind": "carousel", "pinned": n == 0, "caption": caption,
                    "items": [{ "number": 1 }, { "number": 2 }],
                })
            })
            .collect();
        let mut brief = grid_at(0);
        brief.page.as_mut().unwrap().brief = true;
        let told = from(
            &brief,
            finished(
                ExitCode::Ok,
                Some(json!({
                    "username": "someone", "pages_read": 2,
                    "until": { "reached": true, "code": "code7" }, "posts": posts,
                })),
                None,
            ),
        );
        assert_eq!(told.body["count"], 20);
        assert_eq!(told.body["next_offset"], Value::Null, "all twenty fit");
        assert_eq!(
            told.body["items"][0],
            json!({
                "pk": "0", "code": "code0", "taken_at": 1_790_000_000,
                "url": "https://www.instagram.com/p/code0/", "owner": "someone",
                "kind": "carousel", "pinned": true, "item_count": 2,
            })
        );
        assert_eq!(told.body["pages_read"], 2);
        assert_eq!(told.body["until"]["reached"], true);

        // The same read in full takes several pages, held whole, and a page
        // of it is handed back either way.
        let full = from(
            &grid_at(0),
            finished(
                ExitCode::Ok,
                Some(json!({ "username": "someone", "posts": posts })),
                None,
            ),
        );
        let whole = full.whole.expect("held");
        let later = held(&whole, 10, None, true);
        assert_eq!(later.body["items"][0]["pk"], "10");
        assert!(later.body["items"][0].get("caption").is_none());
        assert_eq!(
            held(&whole, 10, None, false).body["items"][0]["caption"],
            caption
        );
    }

    /// No answer about posts holds a media address, wherever it was: the
    /// items', the best tracks', a liker's picture. The post's own link
    /// stays, and so does every key.
    #[test]
    fn no_posts_answer_holds_a_cdn_address() {
        let printed = json!({
            "username": "someone",
            "declared_posts": 30,
            "more": true,
            "posts": [{
                "url": "https://www.instagram.com/p/DTYHCKvDNxy/",
                "caption": "see https://example.com/shop",
                "items": [{
                    "url": "https://scontent-mad1-1.cdninstagram.com/v/t51.jpg?stp=x&oh=y",
                    "best": {
                        "video_url": "https://video.fmad1-1.fna.fbcdn.net/o1/v.mp4?efg=z",
                        "audio_url": "http://127.0.0.1:9/a.mp4"
                    }
                }]
            }]
        });
        let told = from(&grid_at(0), finished(ExitCode::Ok, Some(printed), None));
        let post = &told.body["items"][0];
        assert_eq!(post["url"], "https://www.instagram.com/p/DTYHCKvDNxy/");
        assert_eq!(post["items"][0]["url"], Value::Null);
        assert!(post["items"][0]["best"].get("video_url").is_some());
        assert_eq!(post["items"][0]["best"]["video_url"], Value::Null);
        assert_eq!(post["items"][0]["best"]["audio_url"], Value::Null);
        // A caption is text, not an address, however it ends.
        assert_eq!(post["caption"], "see https://example.com/shop");
        let text = told.body.to_string();
        assert!(
            !text.contains("cdninstagram") && !text.contains("fbcdn"),
            "{text}"
        );
    }

    /// The grid's posts are paged out of its document, which every page
    /// repeats beside them, later pages held with it.
    #[test]
    fn the_grids_posts_are_paged_with_what_the_document_says() {
        let caption = "x".repeat(4_000);
        let posts: Vec<Value> = (0..20)
            .map(|n| json!({ "number": n, "caption": caption }))
            .collect();
        let told = from(
            &grid_at(0),
            finished(
                ExitCode::Ok,
                Some(json!({ "username": "someone", "more": false, "posts": posts })),
                None,
            ),
        );
        assert_eq!(told.body["username"], "someone");
        assert_eq!(told.body["count"], 20);
        let next = told.body["next_offset"].as_u64().expect("another page");
        assert!(next < 20);
        let whole = told.whole.expect("held for the next page");
        let later = held(&whole, next as usize, None, false);
        assert_eq!(later.body["username"], "someone");
        assert_eq!(later.body["items"][0]["number"], next);
        // An account with none prints `"posts": null`, answered as it came.
        let none = from(
            &grid_at(0),
            finished(
                ExitCode::Ok,
                Some(json!({ "username": "someone", "posts": null })),
                None,
            ),
        );
        assert_eq!(none.body["posts"], Value::Null);
    }

    /// A list's plan, for the page at `offset`.
    fn list_at(offset: usize) -> Plan {
        limited_at(offset, None)
    }

    /// A list's plan given `limit`, for the page at `offset`.
    fn limited_at(offset: usize, limit: Option<usize>) -> Plan {
        Plan {
            page: Some(crate::commands::mcp::tools::Page {
                offset,
                of: "the call".into(),
                held_only: false,
                limit,
                rows: None,
                brief: false,
            }),
            ..plan(&[ExitCode::Ok])
        }
    }

    /// `n` accounts, as a list command prints them.
    fn accounts(n: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|n| json!({"pk": n, "username": format!("user{n}")}))
                .collect(),
        )
    }

    fn usernames(body: &Value) -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["username"].as_str().unwrap().to_string())
            .collect()
    }

    /// A refusal whose hint is advice to the person at a terminal is told
    /// to the agent in its own tools' words.
    #[test]
    fn a_hint_for_the_terminal_is_told_to_the_agent_in_its_tools() {
        for (reason, exit, tool) in [
            ("too_many_jobs", ExitCode::Error, "snob_job_cancel"),
            ("no_such_job", ExitCode::Error, "snob_jobs"),
            ("job_stopping", ExitCode::Error, "snob_job "),
            ("nothing_stored", ExitCode::Error, "mode \"auto\""),
            ("stored_moments", ExitCode::Error, "mode \"auto\""),
            ("budget_wait", ExitCode::Interrupted, "snob_status"),
            ("busy", ExitCode::Error, "snob_job_cancel"),
            ("mcp_write_cap", ExitCode::Error, "retry_at"),
            ("walked_elsewhere", ExitCode::Error, "retry_at"),
        ] {
            let told = from(
                &plan(&[ExitCode::Ok]),
                finished(
                    exit,
                    None,
                    Some(json!({"error": {
                        "code": exit.as_str(),
                        "reason": reason,
                        "message": "it was refused",
                        "hint": "run \"snob jobs cancel\", or drop --offline, or --max-wait",
                    }})),
                ),
            );
            assert!(told.failed);
            let error = &told.body["error"];
            assert_eq!(error["code"], reason);
            let hint = error["hint"].as_str().unwrap();
            assert!(hint.contains(tool), "{reason}: {hint}");
            assert!(!hint.contains("snob jobs cancel"), "{reason}: {hint}");
            assert!(!hint.contains("--"), "{reason}: {hint}");
        }
        // A reason with a hint an agent can already act on keeps it.
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::Error,
                None,
                Some(json!({"error": {"code": "error", "reason": "refused_argv", "hint": "wait"}})),
            ),
        );
        assert_eq!(told.body["error"]["hint"], "wait");
    }

    /// The records of the log a failed walk wrote are no part of what the
    /// agent is told: the sentences are.
    #[test]
    fn a_failure_says_its_sentences_not_its_log() {
        let mut walked = finished(ExitCode::RateLimited, Some(accounts(2)), None);
        walked.said = " WARN snob_store::store::rate_budget: account in cooldown \
                       reason=\"rate_limit\" minutes=120\n \
                       WARN snob_ig::client: Instagram pushed back endpoint=\"/api/v1/x/\" \
                       status=429 body={\"message\":\"Please wait a few minutes\"}\n\
                       warning: Instagram asked to slow down\n\
                       warning: the walk stopped after 2 accounts; they are stored\n"
            .into();
        let told = from(&list_at(0), walked);
        assert!(told.failed);
        assert_eq!(
            told.body["error"]["message"],
            "warning: Instagram asked to slow down \
             warning: the walk stopped after 2 accounts; they are stored"
        );
    }

    #[test]
    fn a_long_list_is_handed_back_a_page_at_a_time() {
        let mut read = Vec::new();
        let mut offset = 0;
        let mut kept = None;
        loop {
            let told = from(
                &list_at(offset),
                finished(ExitCode::Ok, Some(accounts(450)), None),
            );
            assert!(!told.failed);
            assert_eq!(told.body["count"], 450, "the whole list's");
            assert_eq!(told.body["offset"], offset);
            let page = usernames(&told.body);
            assert!(page.len() <= PAGE);
            read.extend(page);
            if let Some(whole) = told.whole {
                // What the server holds pages the same as the command did.
                assert_eq!(held(&whole, offset, None, false).body, told.body);
                kept = Some(whole);
            }
            match told.body["next_offset"].as_u64() {
                Some(next) => {
                    assert!(
                        next_page(&told.body, "snob_list")
                            .unwrap()
                            .contains(&format!("\"offset\": {next}"))
                    );
                    offset = usize::try_from(next).unwrap();
                }
                None => break,
            }
        }
        let every: Vec<String> = (0..450).map(|n| format!("user{n}")).collect();
        assert_eq!(read, every, "each account once, in the order served");
        assert_eq!(kept.unwrap().items.len(), 450);
    }

    #[test]
    fn the_last_page_has_no_next_offset() {
        let told = from(
            &list_at(400),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(usernames(&told.body).len(), 50);
        assert!(told.body["next_offset"].is_null());
        assert!(next_page(&told.body, "snob_list").is_none());
        assert!(told.whole.is_none(), "nothing after it to hold for");

        // A list that fits on one page is the same shape, and held for nothing.
        let short = from(&list_at(0), finished(ExitCode::Ok, Some(accounts(3)), None));
        assert_eq!(short.body["count"], 3);
        assert!(short.body["next_offset"].is_null());
        assert!(short.whole.is_none());
    }

    #[test]
    fn a_limit_keeps_the_first_accounts_and_still_says_the_whole_list() {
        // Within one page: no next one, and a sentence saying there are more,
        // so the limit is never read as the list's size.
        let five = from(
            &limited_at(0, Some(5)),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(five.body["total"], 450);
        assert_eq!(five.body["count"], 5);
        assert_eq!(
            usernames(&five.body),
            ["user0", "user1", "user2", "user3", "user4"]
        );
        assert!(five.body["next_offset"].is_null());
        assert!(next_page(&five.body, "snob_list").is_none());
        assert!(limited(&five.body).unwrap().contains("the list has 450"));

        // Across pages: they stop at the limit, and each says both numbers.
        let first = from(
            &limited_at(0, Some(300)),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(first.body["total"], 450);
        assert_eq!(first.body["count"], 300);
        assert_eq!(first.body["next_offset"], PAGE);
        assert!(limited(&first.body).is_none(), "next_page says it instead");
        assert!(
            next_page(&first.body, "snob_list")
                .unwrap()
                .contains("of 300 (the limit; the list has 450)")
        );
        let whole = first.whole.unwrap();
        assert_eq!(whole.items.len(), 450, "the whole list is held");
        let last = held(&whole, PAGE, Some(300), false);
        assert_eq!(usernames(&last.body).len(), 300 - PAGE);
        assert!(last.body["next_offset"].is_null());
        assert_eq!(last.body["total"], 450);

        // A limit past the list is no limit, and nothing more to say.
        let all = from(
            &limited_at(0, Some(10)),
            finished(ExitCode::Ok, Some(accounts(3)), None),
        );
        assert_eq!(
            (all.body["total"].clone(), all.body["count"].clone()),
            (json!(3), json!(3))
        );
        assert!(limited(&all.body).is_none());
    }

    #[test]
    fn a_profiles_mutual_followers_are_handed_back_without_their_pictures() {
        let printed = json!({
            "username": "nora",
            "followed_by": {"count": 1, "accounts": [
                {"pk": 1, "username": "anna", "pfp_url": "https://cdn.example/a.jpg"}
            ], "complete": true},
        });
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Ok, Some(printed), None),
        );
        let anna = &told.body["followed_by"]["accounts"][0];
        assert!(anna.get("pfp_url").is_none(), "{anna}");
        assert_eq!(anna["username"], "anna");
    }

    #[test]
    fn an_account_is_handed_back_without_its_picture() {
        let printed = json!([{
            "pk": 1,
            "username": "anna",
            "full_name": "Anna",
            "is_private": false,
            "is_verified": false,
            "pfp_url": "https://cdn.example/a.jpg?signed=0123456789",
        }]);
        let told = from(
            &list_at(0),
            finished(ExitCode::Ok, Some(printed.clone()), None),
        );
        let item = &told.body["items"][0];
        assert!(item.get("pfp_url").is_none(), "{item}");
        assert_eq!(item["username"], "anna");
        assert_eq!(item["full_name"], "Anna");
        // What a walk kept before it failed is handed back the same way.
        let failed = from(&list_at(0), finished(ExitCode::Error, Some(printed), None));
        assert!(
            failed.body["error"]["partial"]["items"][0]
                .get("pfp_url")
                .is_none()
        );
    }

    #[test]
    fn an_offset_past_the_end_is_refused_with_the_count_and_the_total() {
        for offset in [450, 451, usize::MAX] {
            let told = from(
                &list_at(offset),
                finished(ExitCode::Ok, Some(accounts(450)), None),
            );
            assert!(told.failed, "{offset}");
            assert_eq!(told.body["error"]["code"], "offset_past_end", "{offset}");
            assert_eq!(told.body["error"]["count"], 450, "{offset}");
            let none = Whole {
                items: vec![],
                about: serde_json::Map::new(),
                pictures: Vec::new(),
            };
            assert!(held(&none, offset, None, false).failed, "{offset}");
        }
        // The first page of an empty list is a page, not a refusal.
        let empty = from(&list_at(0), finished(ExitCode::Ok, Some(accounts(0)), None));
        assert!(!empty.failed);
        assert_eq!(empty.body["items"], json!([]));
    }

    #[test]
    fn an_offset_past_a_limit_says_the_limit_is_not_a_page_size() {
        // limit 5, offset 5: once a page of nothing beside count 5.
        let told = from(
            &limited_at(5, Some(5)),
            finished(ExitCode::Ok, Some(accounts(25)), None),
        );
        assert!(told.failed);
        assert_eq!(told.body["error"]["count"], 5);
        assert_eq!(told.body["error"]["total"], 25);
        let message = told.body["error"]["message"].as_str().unwrap();
        assert!(message.contains("not one page"), "{message}");
    }

    #[test]
    fn a_page_of_long_accounts_is_cut_by_its_size_and_still_moves_on() {
        let long = |n: usize, length: usize| {
            Value::Array(
                (0..n)
                    .map(|n| json!({"pk": n, "full_name": "x".repeat(length)}))
                    .collect(),
            )
        };
        let told = from(
            &list_at(0),
            finished(ExitCode::Ok, Some(long(300, 400)), None),
        );
        let shown = told.body["items"].as_array().unwrap().len();
        assert!(shown < PAGE, "{shown}");
        let bytes: usize = told.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.to_string().len())
            .sum();
        assert!(bytes <= PAGE_BYTES, "{bytes}");
        assert_eq!(told.body["next_offset"], shown);

        // One account past the size on its own is a page of one, not none.
        let huge = from(
            &list_at(0),
            finished(ExitCode::Ok, Some(long(2, PAGE_BYTES)), None),
        );
        assert_eq!(huge.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(huge.body["next_offset"], 1);
    }

    #[test]
    fn what_a_walk_stopped_short_read_is_one_page_with_none_after_it() {
        let told = from(
            &list_at(0),
            finished(
                ExitCode::Interrupted,
                Some(accounts(450)),
                Some(json!({"error": {"code": "interrupted"}})),
            ),
        );
        assert!(told.failed);
        let partial = &told.body["error"]["partial"];
        assert_eq!(partial["count"], 450);
        assert_eq!(usernames(partial).len(), PAGE);
        assert!(partial["next_offset"].is_null());
        assert!(told.whole.is_none());
        assert!(next_page(&told.body, "snob_list").is_none());
    }

    #[test]
    fn a_list_from_a_call_that_takes_no_offset_offers_no_next_page() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(told.body["count"], 450);
        assert_eq!(usernames(&told.body).len(), PAGE);
        assert!(told.body["next_offset"].is_null());
        assert!(told.whole.is_none());
    }

    fn finished(code: ExitCode, json: Option<Value>, error: Option<Value>) -> Finished {
        Finished {
            code,
            json,
            text: String::new(),
            error,
            said: "first\nsecond\n".into(),
            served: Vec::new(),
        }
    }

    /// A dry run in a cooldown exits 5 with its estimate printed, which is the
    /// answer asked for; exit 5 with nothing printed is still a refusal.
    #[test]
    fn an_estimate_in_a_cooldown_is_an_answer_and_a_bare_exit_five_is_not() {
        let dry = plan(&[ExitCode::Ok, ExitCode::RateLimited]);
        let estimate =
            json!({"dry_run": true, "sends_now": false, "cooldown_until": 1_800_000_000});
        let told = from(&dry, finished(ExitCode::RateLimited, Some(estimate), None));
        assert!(!told.failed, "{}", told.body);
        assert_eq!(told.body["sends_now"], false);
        assert_eq!(told.body["exit"], "rate_limited");
        assert_eq!(told.body["cooldown_until"], 1_800_000_000);

        let bare = from(&dry, finished(ExitCode::RateLimited, None, None));
        assert!(bare.failed, "{}", bare.body);
        assert_eq!(bare.body["error"]["code"], "rate_limited");
    }

    /// A list served from storage in a cooldown says so, when it was read and
    /// when the cooldown lifts, in the JSON and in a sentence; a list walked
    /// now says it was, and has no sentence.
    #[test]
    fn a_list_served_in_a_cooldown_says_it_is_stored_and_how_old() {
        let mut stored = finished(ExitCode::Ok, Some(accounts(2)), None);
        stored.served = vec![json!({
            "event": "served",
            "list": "followers",
            "from": "stored_in_cooldown",
            "taken_at": 1_793_340_000,
            "cooldown_until": 1_793_347_200,
            "page": 0,
        })];
        let told = from(&list_at(0), stored);
        assert!(!told.failed);
        assert_eq!(told.body["count"], 2);
        assert_eq!(
            told.body["served"],
            json!([{"list": "followers", "from": "stored_in_cooldown", "taken_at": 1_793_340_000}])
        );
        assert_eq!(told.body["cooldown_until"], 1_793_347_200);
        let said = not_read_now(&told.body).expect("a sentence");
        assert!(said.contains("stored at 2026-10-30 06:00 UTC"), "{said}");
        assert!(
            said.contains("cooldown until 2026-10-30 08:00 UTC"),
            "{said}"
        );

        let mut walked = finished(ExitCode::Ok, Some(accounts(2)), None);
        walked.served = vec![json!({
            "list": "followers",
            "from": "walked",
            "taken_at": 1_793_340_000,
            "cooldown_until": null,
        })];
        let told = from(&list_at(0), walked);
        assert_eq!(told.body["served"][0]["from"], "walked");
        assert!(told.body.get("cooldown_until").is_none());
        assert!(not_read_now(&told.body).is_none());
    }

    /// A walk cut short by `max_pages` exits 0 with what it read, and only
    /// its `served` line says it stopped: the answer says it is not the whole
    /// list, how much of it was read, and so does every later page.
    #[test]
    fn a_walk_cut_short_says_it_is_not_the_whole_list() {
        let mut short = finished(ExitCode::Ok, Some(accounts(250)), None);
        short.served = vec![json!({
            "event": "served",
            "list": "followers",
            "from": "walked",
            "taken_at": 1_793_340_000,
            "cooldown_until": null,
            "reason": "page_limit",
            "page": 21,
            "accounts": 250,
            "of": 2500,
        })];
        let told = from(&list_at(0), short);
        assert!(!told.failed);
        assert_eq!(told.body["complete"], false);
        assert_eq!(told.body["count"], 250);
        assert_eq!(
            told.body["served"],
            json!([{
                "list": "followers", "from": "walked", "taken_at": 1_793_340_000,
                "stopped": "page_limit", "read": 250, "of": 2500,
            }])
        );
        let said = incomplete(&told.body).expect("a sentence");
        assert!(said.contains("stopped at max_pages"), "{said}");
        assert!(said.contains("250 of about 2500 accounts read"), "{said}");
        assert!(not_read_now(&told.body).is_none(), "it was read now");

        // The next page, handed back from what the server holds, says it too.
        let whole = told.whole.expect("a page after this one");
        let later = held(&whole, 200, None, false);
        assert_eq!(later.body["complete"], false);
        assert_eq!(later.body["served"], told.body["served"]);
        assert!(incomplete(&later.body).is_some());

        // A whole walk says nothing of the kind.
        let mut whole_walk = finished(ExitCode::Ok, Some(accounts(2)), None);
        whole_walk.served = vec![json!({
            "list": "followers", "from": "walked", "taken_at": 1, "reason": "completed",
            "accounts": 2, "of": 2,
        })];
        let told = from(&list_at(0), whole_walk);
        assert!(told.body.get("complete").is_none());
        assert!(told.body["served"][0].get("stopped").is_none());
        assert!(incomplete(&told.body).is_none());
    }

    /// A crossing whose base list was cut short found its count among what
    /// was read, against the whole other list: some results are missing and
    /// the ones given are right, and its count is not what was read.
    #[test]
    fn a_crossing_cut_short_says_results_are_missing() {
        let mut short = finished(ExitCode::Ok, Some(accounts(2)), None);
        short.served = vec![
            json!({
                "list": "following", "from": "walked", "taken_at": 1_793_340_000,
                "reason": "completed", "accounts": 30, "of": 30,
            }),
            json!({
                "list": "followers", "from": "walked", "taken_at": 1_793_340_000,
                "reason": "page_limit", "accounts": 12, "of": 25,
            }),
        ];
        let told = from(&list_at(0), short);
        assert_eq!(told.body["complete"], false);
        assert_eq!(told.body["count"], 2);
        let said = incomplete(&told.body).expect("a sentence");
        assert!(said.contains("12 of about 25 accounts read"), "{said}");
        assert!(said.contains("some results are missing"), "{said}");
        assert!(!said.contains("count is what was read"), "{said}");
    }

    /// What is read from storage because the call asked for it is dated as
    /// any other stored list is.
    #[test]
    fn a_list_read_from_storage_as_asked_is_dated() {
        let mut stored = finished(ExitCode::Ok, Some(accounts(2)), None);
        stored.served = vec![json!({
            "list": "fans", "from": "stored", "taken_at": 1_793_340_000,
            "cooldown_until": null, "reason": "completed",
        })];
        let told = from(&list_at(0), stored);
        assert_eq!(told.body["served"][0]["taken_at"], 1_793_340_000);
        let said = not_read_now(&told.body).expect("a sentence");
        assert!(said.contains("Read from what is stored"), "{said}");
        assert!(said.contains("stored at 2026-10-30 06:00 UTC"), "{said}");
    }

    /// A push-back met by the call itself has started a cooldown that the
    /// failure does not date: the agent is pointed at `snob_status`, in
    /// place of the command's own advice to type it again later. One that
    /// an existing cooldown refused is dated, and keeps its own hint.
    #[test]
    fn a_push_back_met_now_points_at_snob_status() {
        let met = finished(
            ExitCode::RateLimited,
            None,
            Some(json!({"error": {
                "code": "rate_limited",
                "message": "Instagram is throttling requests",
                "cooldown_until": null,
                "hint": null,
            }})),
        );
        let told = from(&list_at(0), met);
        assert!(told.failed);
        assert_eq!(told.body["error"]["hint"], PUSHED_BACK);

        let refused = finished(
            ExitCode::RateLimited,
            None,
            Some(json!({"error": {
                "code": "rate_limited",
                "message": "in cooldown",
                "cooldown_until": 1_793_347_200,
                "hint": "wait",
            }})),
        );
        let told = from(&list_at(0), refused);
        assert_eq!(told.body["error"]["hint"], "wait");
    }

    #[test]
    fn a_list_is_an_object_with_its_items() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Ok, Some(json!([{"username": "a"}])), None),
        );
        assert!(!told.failed);
        assert_eq!(told.body["count"], 1);
    }

    #[test]
    fn a_failure_carries_the_commands_own_error_object() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::RateLimited,
                None,
                Some(json!({"viewer": null, "error": {"code": "rate_limited", "hint": "wait"}})),
            ),
        );
        assert!(told.failed);
        assert_eq!(told.body["error"]["code"], "rate_limited");
        assert_eq!(told.body["exit"], "rate_limited");
    }

    #[test]
    fn a_reason_the_command_gave_is_the_code_the_agent_reads() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::Error,
                None,
                Some(json!({"error": {"code": "error", "reason": "busy", "message": "held"}})),
            ),
        );
        assert!(told.failed);
        assert_eq!(told.body["error"]["code"], "busy");
        assert_eq!(told.body["exit"], "error");
        // And none given changes nothing.
        let plain = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::Error,
                None,
                Some(json!({"error": {"code": "error", "reason": null}})),
            ),
        );
        assert_eq!(plain.body["error"]["code"], "error");
    }

    #[test]
    fn a_failure_with_no_error_object_says_its_last_lines() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Error, None, None),
        );
        // Through `printable`, which turns the line break into a space.
        assert_eq!(told.body["error"]["message"], "first second");
    }

    #[test]
    fn a_code_the_plan_counts_as_an_answer_is_not_a_failure() {
        let told = from(
            &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
            finished(ExitCode::RateLimited, Some(json!({"cooldown": {}})), None),
        );
        assert!(!told.failed);
        assert_eq!(told.body["exit"], "rate_limited");
    }

    #[test]
    fn an_error_line_whose_error_is_not_an_object_is_still_an_answer() {
        for error in [json!("x"), json!(7), json!(["x"]), json!(null)] {
            let told = from(
                &plan(&[ExitCode::Ok]),
                finished(
                    ExitCode::Error,
                    // With a partial result too, which is the field that was
                    // set by indexing.
                    Some(json!([{"username": "a"}])),
                    Some(json!({ "error": error.clone() })),
                ),
            );
            assert!(told.failed, "{error}");
            assert_eq!(told.body["error"]["code"], "error", "{error}");
            assert!(told.body["error"]["partial"]["items"].is_array(), "{error}");
        }
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Error, None, Some(json!({"error": "x"}))),
        );
        assert_eq!(told.body["error"]["message"], "x");
    }

    #[test]
    fn a_document_that_is_not_an_object_is_put_under_a_key() {
        for document in [json!("a string"), json!(3), json!(true), json!(null)] {
            let told = from(
                &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
                finished(ExitCode::Ok, Some(document.clone()), None),
            );
            assert!(!told.failed, "{document}");
            assert_eq!(told.body["value"], cleaned(document.clone()));
            // Under a code the plan counts as an answer but that is not 0,
            // which is where `exit` was set by indexing: no answer without an
            // object, and nothing indexed into the document either way.
            let told = from(
                &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
                finished(ExitCode::RateLimited, Some(document.clone()), None),
            );
            assert!(told.failed, "{document}");
            assert_eq!(told.body["error"]["partial"], cleaned(document.clone()));
            assert_eq!(told.body["exit"], "rate_limited", "{document}");
        }
    }

    #[test]
    fn no_session_tells_the_agent_that_the_person_signs_in() {
        // `status --json` with no session: an answer, its error object given
        // the hint in place of the one meant for a person.
        let status = from(
            &plan(&[ExitCode::Ok, ExitCode::NoSession, ExitCode::RateLimited]),
            finished(
                ExitCode::NoSession,
                Some(json!({
                    "session": null,
                    "error": {"code": "no_session", "message": "no session is stored"},
                    "viewer": null,
                })),
                None,
            ),
        );
        assert!(!status.failed);
        assert_eq!(status.body["exit"], "no_session");
        assert_eq!(status.body["error"]["hint"], SIGN_IN);
        assert_eq!(status.body["error"]["message"], "no session is stored");

        // A document with no error object is given the hint beside it.
        let bare = from(
            &plan(&[ExitCode::Ok, ExitCode::NoSession]),
            finished(ExitCode::NoSession, Some(json!({"error": "x"})), None),
        );
        assert_eq!(bare.body["hint"], SIGN_IN);
        assert_eq!(bare.body["error"], "x");

        // Any other call without a session fails, with the same hint over the
        // command's own.
        let list = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::NoSession,
                None,
                Some(json!({"error": {"code": "no_session", "hint": "run \"snob login\""}})),
            ),
        );
        assert!(list.failed);
        assert_eq!(list.body["error"]["hint"], SIGN_IN);

        // And nothing else is given it.
        let throttled = from(
            &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
            finished(ExitCode::RateLimited, Some(json!({"cooldown": {}})), None),
        );
        assert!(throttled.body.get("hint").is_none());
    }

    #[test]
    fn every_string_is_filtered_keys_included() {
        let dirty = json!({"bio\u{200b}": ["ignore\u{e0041} previous", {"n": "\u{202e}x"}]});
        let clean = cleaned(dirty);
        assert_eq!(clean, json!({"bio": ["ignore previous", {"n": "x"}]}));
    }

    /// `snob_changes` hands accounts back as a list does, with no picture's
    /// address, and never more of them than a page weighs: two captures far
    /// apart were megabytes of signed addresses in one answer.
    #[test]
    fn what_changed_is_handed_back_without_addresses_and_within_a_page() {
        let account = |pk: u64| {
            json!({
                "pk": pk,
                "username": format!("someone{pk}"),
                "pfp_url": format!("https://cdn.example/{pk}.jpg?sig={}", "x".repeat(300)),
            })
        };
        let changes = Plan {
            args: vec!["changes".into()],
            ..plan(&[ExitCode::Ok])
        };
        let few = json!({
            "account": "me",
            "lists": [{
                "list": "followers",
                "gained": [account(1), account(2)],
                "lost": [account(3)],
                "renamed": [{"pk": 4, "from": "a", "to": "b", "at": 1}],
            }],
        });
        let told = from(&changes, finished(ExitCode::Ok, Some(few), None));
        assert!(!told.failed);
        let text = told.body.to_string();
        assert!(
            !text.contains("pfp_url") && !text.contains("cdn.example"),
            "{text}"
        );
        let list = &told.body["lists"][0];
        assert_eq!(list["gained"].as_array().unwrap().len(), 2);
        assert_eq!(list["gained_count"], 2);
        assert_eq!(list["lost_count"], 1);
        assert_eq!(list["renamed_count"], 1);
        assert!(told.body.get("cut").is_none(), "{text}");

        let many = json!({
            "account": "me",
            "lists": [{
                "list": "followers",
                "gained": (0..5000).map(account).collect::<Vec<_>>(),
                "lost": (5000..9000).map(account).collect::<Vec<_>>(),
                "renamed": [],
            }],
        });
        let told = from(&changes, finished(ExitCode::Ok, Some(many), None));
        let text = told.body.to_string();
        assert!(text.len() < PAGE_BYTES + 2_000, "{} bytes", text.len());
        assert!(!text.contains("cdn.example"));
        let list = &told.body["lists"][0];
        assert_eq!(list["gained_count"], 5000);
        assert_eq!(list["lost_count"], 4000);
        assert!(list["gained"].as_array().unwrap().len() < 5000);
        assert_eq!(told.body["cut"], CHANGES_CUT);

        // What is no document of changes passes as it came, nothing indexed.
        for odd in [
            json!("text"),
            json!({"lists": "none"}),
            json!({"lists": [1, {"gained": 2}]}),
        ] {
            let told = from(&changes, finished(ExitCode::Ok, Some(odd.clone()), None));
            assert!(!told.failed, "{odd}");
        }
    }
}
