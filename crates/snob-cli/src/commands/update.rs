//! `snob update`: the latest release, now, the way snob was installed.
//!
//! One of the listed exceptions to "commands take an App": it reads no
//! account and sends nothing to Instagram. It asks GitHub (`update::github`),
//! asks the person before changing anything (`-y` answers in advance), and
//! then lets the package manager upgrade snob, replaces the binary itself
//! where the install script put it, or, for the `.deb`, runs or prints the
//! `apt` command only root can run. Unlike the automatic update, it does not
//! wait for a cold start: a person asked for it now, and a snob already
//! running keeps the file it started from (on Windows, the renamed `.old`).
//!
//! The rest of the updater's choices are `update`'s; this is the wording.

use anyhow::Result;
use snob_store::paths::AppPaths;

use crate::cli::{AutoUpdate, UpdateArgs};
use crate::exit::{ExitCode, ExitError};
use crate::update::how::{self, Applies, Method};
use crate::update::state::{self, Auto, Files};
use crate::update::version::Version;
use crate::update::{self};
use crate::update::{check, note_applied, note_failure, origin, put_in_place, stage, this_exe};

pub async fn run(args: UpdateArgs, paths: &AppPaths, sandboxed: bool) -> Result<ExitCode> {
    let files = Files::of(paths);
    if let Some(mode) = args.auto {
        let auto = match mode {
            AutoUpdate::Off => Auto::Off,
            AutoUpdate::Notify => Auto::Notify,
            AutoUpdate::Download => Auto::Download,
            AutoUpdate::Apply => Auto::Apply,
        };
        state::change(&files, |s| s.auto = auto)?;
        if !args.json {
            crate::ui::info(match auto {
                Auto::Off => "snob no longer checks for updates on its own.",
                Auto::Notify => {
                    "snob checks for a new release once a day and says when one is out."
                }
                Auto::Download => {
                    "snob checks once a day and downloads a new release; \"snob update\" applies it."
                }
                Auto::Apply => {
                    "snob checks once a day, downloads a new release, and applies it the next \
                     time it starts with no other snob running."
                }
            });
        }
        return finish(&args, paths, sandboxed, None);
    }
    if args.status {
        if !args.json {
            crate::ui::say!("{}", prose(&update::report(paths, sandboxed)));
        }
        return finish(&args, paths, sandboxed, None);
    }

    update::may_update_by_hand(sandboxed)?;
    // What the report at the end calls installed: this binary's version,
    // until one is put in its place.
    let mut installed = Version::current();
    let method = how::this_binary();
    let current = env!("CARGO_PKG_VERSION");
    let newer = match check(&files, &origin()).await {
        Ok(newer) => newer,
        Err(e) => {
            note_failure(&files, &format!("{e:#}"));
            return Err(ExitError::new(
                ExitCode::Error,
                format!("GitHub could not be asked about the latest release: {e:#}"),
            )
            .into());
        }
    };
    let Some(newer) = newer else {
        if !args.json {
            crate::ui::info(&format!("snob {current} is the latest release."));
        }
        return finish(&args, paths, sandboxed, None);
    };
    if args.check {
        if !args.json {
            crate::ui::info(&format!(
                "snob {newer} is out; this is {current}. \"snob update\" updates it."
            ));
        }
        return finish(&args, paths, sandboxed, None);
    }

    let how = match method.applies() {
        Applies::Never => {
            return Err(ExitError::new(
                ExitCode::Error,
                "snob cannot tell how it was installed here, so it leaves itself alone",
            )
            .with_hint(
                "update it the way you installed it, or install it again with the script in the \
                 README",
            )
            .into());
        }
        Applies::Itself => String::new(),
        Applies::Manager | Applies::ByHand => match method.by_hand(None) {
            Some(command) if method != Method::Deb => format!(" with \"{command}\""),
            _ => " with apt".to_string(),
        },
    };
    let shown = format!("updating snob from {current} to {newer}{how}");
    if !args.consent.yes {
        if !crate::ui::can_be_asked() {
            return Err(crate::report::refuse_unattended(
                format!("{shown} needs confirmation"),
                "Pass -y to confirm in advance.".to_string(),
            ));
        }
        let question = format!("Update snob from {current} to {newer}{how}?");
        if !crate::ui::confirm(&question, true)? {
            return Err(crate::report::refuse_declined(&shown));
        }
    }

    let done = apply_now(&files, &method, newer).await;
    match done {
        Ok(Done::Applied) => {
            installed = Some(newer);
            note_applied(&files, &newer.to_string());
            // Said here, so the next start does not say it again.
            state::change(&files, |s| {
                if let Some(applied) = &mut s.applied {
                    applied.told = true;
                }
                s.told = Some(newer.to_string());
            })?;
            if !args.json {
                crate::ui::info(&format!("snob was updated from {current} to {newer}."));
            }
        }
        Ok(Done::ByHand(command)) => {
            if !args.json {
                crate::ui::info(&format!(
                    "snob {newer} is downloaded. Install it with: {command}"
                ));
            }
        }
        Err(e) => {
            note_failure(&files, &format!("{e:#}"));
            return Err(
                ExitError::new(ExitCode::Error, format!("snob was not updated: {e:#}")).into(),
            );
        }
    }
    finish(&args, paths, sandboxed, installed)
}

/// What came of applying a version.
enum Done {
    Applied,
    /// Downloaded, for a person to install with this command.
    ByHand(String),
}

async fn apply_now(files: &Files, method: &Method, newer: Version) -> Result<Done> {
    match method.applies() {
        Applies::Itself => {
            crate::ui::info(&format!("Downloading snob {newer}..."));
            stage(files, &origin(), newer, method).await?;
            let staged = state::load(files)
                .staged
                .ok_or_else(|| anyhow::anyhow!("nothing was downloaded"))?;
            put_in_place(&this_exe()?, &staged)?;
            Ok(Done::Applied)
        }
        Applies::Manager => {
            let mut steps = method.prefetch();
            steps.extend(method.apply(&newer.to_string()));
            tokio::task::spawn_blocking(move || {
                crate::update::apply::run_steps(&steps, &crate::update::apply::Output::Shown)
            })
            .await??;
            Ok(Done::Applied)
        }
        Applies::ByHand => {
            crate::ui::info(&format!("Downloading snob {newer}..."));
            stage(files, &origin(), newer, method).await?;
            let deb = state::load(files)
                .staged
                .and_then(|s| s.file)
                .ok_or_else(|| anyhow::anyhow!("nothing was downloaded"))?;
            let command = method
                .by_hand(Some(&deb))
                .unwrap_or_else(|| format!("sudo apt install {}", deb.display()));
            if !crate::ui::can_be_asked() {
                return Ok(Done::ByHand(command));
            }
            let step = how::Step {
                program: "sudo".into(),
                args: vec!["apt".into(), "install".into(), deb.into_os_string()],
                env: Vec::new(),
            };
            tokio::task::spawn_blocking(move || {
                crate::update::apply::run_steps(&[step], &crate::update::apply::Output::Shown)
            })
            .await??;
            Ok(Done::Applied)
        }
        Applies::Never => anyhow::bail!("this install is not one snob updates"),
    }
}

/// With `--json`, the updater's state, on standard output, `installed`
/// being the version now in place when this run put one there.
fn finish(
    args: &UpdateArgs,
    paths: &AppPaths,
    sandboxed: bool,
    installed: Option<Version>,
) -> Result<ExitCode> {
    if args.json {
        let report = match installed {
            Some(installed) => update::report_as(paths, sandboxed, Some(installed)),
            None => update::report(paths, sandboxed),
        };
        crate::ui::say!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(ExitCode::Ok)
}

/// `--status` in sentences.
fn prose(report: &serde_json::Value) -> String {
    let text = |key: &str| report[key].as_str().unwrap_or_default().to_string();
    let mut lines = vec![format!(
        "snob {}, installed with {}.",
        text("current"),
        match text("method").as_str() {
            "homebrew" => "Homebrew",
            "scoop" => "Scoop",
            "winget" => "WinGet",
            "cargo" => "Cargo",
            "deb" => "the .deb",
            "script" => "the install script",
            "dev" => "Cargo from source (a build, never updated)",
            _ => "something snob does not recognize",
        }
    )];
    lines.push(match (report["enabled"].as_bool(), text("auto").as_str()) {
        (Some(false), _) => "Automatic updates: off here.".to_string(),
        (_, "off") => "Automatic updates: off.".to_string(),
        (_, "notify") => "Automatic updates: says when a release is out.".to_string(),
        (_, "download") => "Automatic updates: downloads a release.".to_string(),
        _ => "Automatic updates: downloads a release and applies it on a cold start.".to_string(),
    });
    if let Some(at) = report["last_check"]["at_iso"].as_str() {
        lines.push(format!("Last checked: {at}."));
    }
    if let Some(available) = report["available"].as_str() {
        let staged = report["staged"]["version"].as_str() == Some(available);
        lines.push(match (staged, report["by_hand"].as_str()) {
            (true, _) => format!("snob {available} is downloaded and waiting to be applied."),
            (false, Some(command)) => format!("snob {available} is out. Update with: {command}"),
            (false, None) => format!("snob {available} is out."),
        });
    }
    if let Some(from) = report["applied"]["from"].as_str() {
        lines.push(format!(
            "Last update: from {from} to {}, at {}.",
            report["applied"]["to"].as_str().unwrap_or_default(),
            report["applied"]["at_iso"].as_str().unwrap_or_default()
        ));
    }
    if let Some(message) = report["error"]["message"].as_str() {
        lines.push(format!("Last problem: {message}"));
    }
    lines.join("\n")
}
