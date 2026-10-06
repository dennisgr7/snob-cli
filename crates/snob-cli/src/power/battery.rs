//! Whether the machine is about to run out of battery.
//!
//! **One question, for the monitor**: is the battery critical, which is when
//! a run started now would be cut off by the system hibernating or shutting
//! down under it. Not "on battery", which is most of a laptop's life and no
//! reason for a run that was asked for to wait; not a level of the user's own
//! choosing either. The scheduled monitor holds a due run back while the
//! answer is yes and runs it when power comes back, and a run in progress
//! stops between two pages, where it can be picked up again
//! (`commands::watch`).
//!
//! **Critical** is the system's own word where it has one (Windows), and
//! otherwise discharging at [`CRITICAL_PERCENT`] or less, which is where the
//! systems' own critical actions sit by default. A machine with no battery,
//! or whose battery cannot be read, is never critical.

/// At or under this, a battery that is discharging is critical where the
/// system does not say so itself.
///
/// Five: the default critical level of Windows' own power plans, around which
/// macOS forces a sleep and GNOME's and KDE's power managers act. Above it a
/// run has room to finish a page and save it; below it the system is about to
/// take the decision itself.
pub const CRITICAL_PERCENT: u8 = 5;

/// What the battery says, read once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Running on mains power.
    pub on_power: bool,
    /// The charge left, when the system says.
    pub percent: Option<u8>,
    /// The system calls the battery critical, or it is discharging at
    /// [`CRITICAL_PERCENT`] or less.
    pub critical: bool,
}

impl Battery {
    /// What a battery discharging at `percent`, or plugged in, adds up to.
    fn reading(
        on_power: bool,
        discharging: bool,
        percent: Option<u8>,
        said_critical: bool,
    ) -> Self {
        let low = percent.is_some_and(|p| p <= CRITICAL_PERCENT);
        Self {
            on_power,
            percent,
            critical: !on_power && (said_critical || (discharging && low)),
        }
    }
}

/// Whether the battery is critical right now. `false` on a machine without
/// one, or where it cannot be read.
pub fn critical() -> bool {
    read().is_some_and(|battery| battery.critical)
}

/// The machine's battery, or `None` when it has none or it cannot be read.
pub fn read() -> Option<Battery> {
    #[cfg(windows)]
    return windows();
    #[cfg(target_os = "linux")]
    return linux::read(std::path::Path::new("/sys/class/power_supply"));
    #[cfg(target_os = "macos")]
    return macos::read();
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    None
}

/// `GetSystemPowerStatus`, which says itself when the battery is critical.
#[cfg(windows)]
fn windows() -> Option<Battery> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    /// `BatteryFlag`'s bits, as `GetSystemPowerStatus` documents them.
    const CRITICAL: u8 = 4;
    const CHARGING: u8 = 8;
    const NO_BATTERY: u8 = 128;
    const UNKNOWN: u8 = 255;

    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: the pointer is to a local structure of the type the call fills.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return None;
    }
    if status.BatteryFlag == UNKNOWN || status.BatteryFlag & NO_BATTERY != 0 {
        return None;
    }
    let percent = (status.BatteryLifePercent <= 100).then_some(status.BatteryLifePercent);
    Some(Battery::reading(
        status.ACLineStatus == 1,
        status.BatteryFlag & CHARGING == 0,
        percent,
        status.BatteryFlag & CRITICAL != 0,
    ))
}

/// The kernel's power supply class, read from its files: no daemon, no bus.
#[cfg(any(target_os = "linux", test))]
mod linux {
    use std::path::Path;

    use super::Battery;

    /// Every supply under `root`: on power when any mains or USB supply is
    /// online, discharging when a system battery says so. A battery of a
    /// device, a mouse or a headset, has `scope` `Device` and is not the
    /// machine's.
    pub(super) fn read(root: &Path) -> Option<Battery> {
        let mut on_power = false;
        let mut battery = None;
        for supply in std::fs::read_dir(root).ok()?.flatten() {
            let path = supply.path();
            let field = |name: &str| {
                std::fs::read_to_string(path.join(name))
                    .ok()
                    .map(|value| value.trim().to_string())
            };
            match field("type").as_deref() {
                Some("Mains" | "USB") => on_power |= field("online").as_deref() == Some("1"),
                Some("Battery") if field("scope").as_deref() != Some("Device") => {
                    let discharging = field("status").as_deref() == Some("Discharging");
                    let percent = field("capacity").and_then(|c| c.parse::<u8>().ok());
                    let said_critical = field("capacity_level").as_deref() == Some("Critical");
                    battery.get_or_insert((discharging, percent, said_critical));
                }
                _ => {}
            }
        }
        let (discharging, percent, said_critical) = battery?;
        Some(Battery::reading(
            on_power,
            discharging,
            percent,
            said_critical && discharging,
        ))
    }
}

/// `pmset -g batt`, the system's own report, read at most once a minute: it is
/// a process started, and a run asks between every two pages.
#[cfg(any(target_os = "macos", test))]
mod macos {
    use super::Battery;

    #[cfg(target_os = "macos")]
    pub(super) fn read() -> Option<Battery> {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};

        const FRESH: Duration = Duration::from_secs(60);
        static LAST: Mutex<Option<(Instant, Option<Battery>)>> = Mutex::new(None);

        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, battery)) = *last
            && at.elapsed() < FRESH
        {
            return battery;
        }
        let battery = std::process::Command::new("/usr/bin/pmset")
            .args(["-g", "batt"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| parse(&String::from_utf8_lossy(&out.stdout)));
        *last = Some((Instant::now(), battery));
        battery
    }

    /// What `pmset -g batt` printed, with a tab before the percentage:
    ///
    /// ```text
    /// Now drawing from 'Battery Power'
    ///  -InternalBattery-0 (id=4653155)    4%; discharging; 0:09 remaining present: true
    /// ```
    pub(super) fn parse(report: &str) -> Option<Battery> {
        let on_power = report.contains("'AC Power'");
        let line = report.lines().find(|l| l.contains("InternalBattery"))?;
        let percent = line
            .split(['\t', ' ', ';'])
            .find_map(|word| word.strip_suffix('%'))
            .and_then(|n| n.parse::<u8>().ok());
        let discharging = line.contains("discharging");
        Some(Battery::reading(on_power, discharging, percent, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(root: &std::path::Path, name: &str, fields: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir(&dir).unwrap();
        for (field, value) in fields {
            std::fs::write(dir.join(field), format!("{value}\n")).unwrap();
        }
    }

    #[test]
    fn a_laptop_draining_at_the_end_is_critical() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "AC", &[("type", "Mains"), ("online", "0")]);
        supply(
            root.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging"),
                ("capacity", "4"),
            ],
        );
        let battery = linux::read(root.path()).unwrap();
        assert!(battery.critical);
        assert_eq!(battery.percent, Some(4));
    }

    #[test]
    fn plugged_in_or_above_the_line_is_not_critical() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        supply(
            root.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Charging"),
                ("capacity", "3"),
            ],
        );
        assert!(!linux::read(root.path()).unwrap().critical, "plugged in");

        let root = tempfile::tempdir().unwrap();
        supply(
            root.path(),
            "BAT0",
            &[
                ("type", "Battery"),
                ("status", "Discharging"),
                ("capacity", "6"),
            ],
        );
        assert!(
            !linux::read(root.path()).unwrap().critical,
            "above the line"
        );
    }

    /// A mouse's battery is not the machine's, and a desktop has none.
    #[test]
    fn a_device_battery_or_none_at_all_is_no_battery() {
        let root = tempfile::tempdir().unwrap();
        supply(root.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        supply(
            root.path(),
            "hidpp_battery_0",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("status", "Discharging"),
                ("capacity", "2"),
            ],
        );
        assert_eq!(linux::read(root.path()), None);
        assert_eq!(linux::read(&root.path().join("nowhere")), None);
    }

    #[test]
    fn pmset_is_read_as_the_mac_says_it() {
        let draining = "Now drawing from 'Battery Power'\n \
                        -InternalBattery-0 (id=4653155)\t4%; discharging; 0:09 remaining present: true\n";
        let battery = macos::parse(draining).unwrap();
        assert!(battery.critical);
        assert_eq!(battery.percent, Some(4));

        let charging = "Now drawing from 'AC Power'\n \
                        -InternalBattery-0 (id=4653155)\t3%; charging; 2:10 remaining present: true\n";
        assert!(!macos::parse(charging).unwrap().critical);

        assert_eq!(
            macos::parse("Now drawing from 'AC Power'\n"),
            None,
            "a Mac mini"
        );
    }
}
