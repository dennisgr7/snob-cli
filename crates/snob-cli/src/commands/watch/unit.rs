//! `snob watch unit`: what to give a scheduler, printed and never installed.
//!
//! **Every unit runs `snob watch once --when-due` every quarter of an hour**,
//! whatever the schedule is, and the run decides whether it is due
//! (`once::is_due`, the evaluator the scheduled mode sleeps on). Translating
//! the schedule into each scheduler's own language would copy the rules of
//! `snob_core::watch::schedule` five times, and some of them cannot be said
//! there at all: cron has no "every two weeks on Monday", systemd's calendar
//! has no cron's OR between the day of the month and the day of the week,
//! and none of them knows the jitter. A run asked for and not due reads no
//! session and sends nothing, so asking often costs nothing; the schedule
//! stays in `watch.toml`, where `watch setup` and `watch status` read it.
//!
//! Nothing here writes a file, enables a unit or calls a scheduler: the
//! person reads what it prints and puts it where it says.

use anyhow::Result;

use crate::cli::{UnitKind, WatchUnitArgs};
use crate::exit::ExitCode;
use crate::ui;

/// How often every unit asks. A quarter of an hour is how late a run on a
/// calendar may start, against 96 starts a day that send nothing.
const ASK_EVERY_MINUTES: u32 = 15;

/// What every unit runs, after the binary.
const RUN: [&str; 3] = ["watch", "once", "--when-due"];

pub(super) fn unit(args: WatchUnitArgs) -> Result<ExitCode> {
    let exe = crate::exe::to_print(
        "a scheduler started from it stops working once an upgrade removes that directory; \
         print the unit again after an upgrade",
    )?;
    let exe = exe.to_string_lossy();
    let kind = args.kind.unwrap_or_else(UnitKind::of_this_platform);
    ui::say!("{}", render(kind, &exe).trim_end());
    ui::info(
        "The schedule stays in watch.toml: each run asks whether it is due, and one that is \
         not sends nothing. Add --account <name> before \"watch\" if this computer signs in \
         more than one account.",
    );
    Ok(ExitCode::Ok)
}

/// The unit for `kind`, running `exe`.
fn render(kind: UnitKind, exe: &str) -> String {
    match kind {
        UnitKind::Systemd => systemd(exe),
        UnitKind::Cron => cron(exe),
        UnitKind::Launchd => launchd(exe),
        UnitKind::Schtasks => schtasks(exe),
    }
}

/// A user service and its timer. `SuccessExitStatus` names 5 (cooling down)
/// and 130 (held for a critical battery), which are a run declining to run,
/// not a failing unit; `Persistent` runs a start the machine slept through.
fn systemd(exe: &str) -> String {
    // systemd reads `%` as a specifier, `$` as a variable, quoted or not,
    // and splits on spaces outside quotes.
    let quoted = format!(
        "\"{}\"",
        exe.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    );
    format!(
        "# ~/.config/systemd/user/snob-watch.service\n\
         [Unit]\n\
         Description=snob monitor, when watch.toml says a run is due\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={quoted} {run}\n\
         SuccessExitStatus=5 130\n\
         Nice=10\n\
         \n\
         # ~/.config/systemd/user/snob-watch.timer\n\
         [Unit]\n\
         Description=Ask snob every {ASK_EVERY_MINUTES} minutes whether a run is due\n\
         \n\
         [Timer]\n\
         OnCalendar=*:0/{ASK_EVERY_MINUTES}\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n\
         \n\
         # Then: systemctl --user daemon-reload\n\
         #       systemctl --user enable --now snob-watch.timer\n\
         # and, to run while you are logged out: loginctl enable-linger\n",
        run = RUN.join(" "),
    )
}

/// One crontab line. cron reads `%` as a line break, and runs the line in
/// `sh`, so the path is single-quoted.
fn cron(exe: &str) -> String {
    let quoted = format!("'{}'", exe.replace('\'', r"'\''").replace('%', r"\%"));
    format!(
        "# crontab -e, and add:\n\
         */{ASK_EVERY_MINUTES} * * * * {quoted} {run}\n\
         # cron starts it with no session bus, where the system keyring may be out of\n\
         # reach: a session saved with \"snob login --no-keyring\" works there.\n",
        run = RUN.join(" "),
    )
}

/// A launch agent: started in the user's session, so the Keychain is open.
fn launchd(exe: &str) -> String {
    let xml = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let arguments: String = std::iter::once(exe)
        .chain(RUN)
        .map(|argument| format!("        <string>{}</string>\n", xml(argument)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!-- ~/Library/LaunchAgents/dev.dennisgr7.snob.watch.plist\n     \
         Then: launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.dennisgr7.snob.watch.plist -->\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n    \
         <key>Label</key>\n    \
         <string>dev.dennisgr7.snob.watch</string>\n    \
         <key>ProgramArguments</key>\n    \
         <array>\n\
         {arguments}    \
         </array>\n    \
         <key>StartInterval</key>\n    \
         <integer>{seconds}</integer>\n    \
         <key>ProcessType</key>\n    \
         <string>Background</string>\n    \
         <key>LowPriorityIO</key>\n    \
         <true/>\n\
         </dict>\n\
         </plist>\n",
        seconds = ASK_EVERY_MINUTES * 60,
    )
}

/// One `schtasks` command, for the user who types it, while signed in: the
/// keyring snob uses there is this user's Credential Manager.
fn schtasks(exe: &str) -> String {
    format!(
        "rem Run once, in a terminal of the user snob signs in as:\n\
         schtasks /Create /TN \"snob watch\" /SC MINUTE /MO {ASK_EVERY_MINUTES} /F /TR \
         \"\\\"{}\\\" {run}\"\n",
        exe.replace('"', ""),
        run = RUN.join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/snob 100%/bin/snob";
    const DOLLAR: &str = "/opt/$HOME/snob";

    #[test]
    fn every_unit_runs_the_same_command_on_the_same_cadence() {
        for kind in [
            UnitKind::Systemd,
            UnitKind::Cron,
            UnitKind::Launchd,
            UnitKind::Schtasks,
        ] {
            let unit = render(kind, "/usr/bin/snob");
            assert!(unit.contains("/usr/bin/snob"), "{unit}");
            assert!(
                unit.contains("watch once --when-due")
                    || unit.contains("<string>--when-due</string>"),
                "{unit}"
            );
            assert!(unit.contains("15") || unit.contains("900"), "{unit}");
        }
    }

    /// A `%` in the path is a specifier to systemd and a line break to cron.
    #[test]
    fn the_path_is_quoted_as_each_scheduler_reads_it() {
        assert!(systemd(EXE).contains("ExecStart=\"/opt/snob 100%%/bin/snob\" watch once"));
        assert!(systemd(DOLLAR).contains("ExecStart=\"/opt/$$HOME/snob\" watch once"));
        assert!(cron(EXE).contains("'/opt/snob 100\\%/bin/snob' watch once"));
        assert!(launchd("/a&b/snob").contains("<string>/a&amp;b/snob</string>"));
        assert!(
            schtasks(r"C:\Program Files\snob\snob.exe")
                .contains(r#"/TR "\"C:\Program Files\snob\snob.exe\" watch once --when-due""#)
        );
    }
}
