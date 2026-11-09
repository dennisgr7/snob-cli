//! How this copy of snob was installed, which decides how it is updated.
//!
//! Read from where the running binary really is (its path with every link
//! followed), the way `exe::stable_exe` finds the path a package manager
//! keeps: each manager puts its programs somewhere of its own, and that is
//! the one thing every install leaves without being asked. **A guess that
//! could be wrong is never acted on as a manager's**: a path that matches no
//! rule is a copy snob may replace itself only when its directory is this
//! user's to write ([`Method::Script`], what the install scripts leave), and
//! otherwise is [`Method::Unknown`], which is only told about.
//!
//! What is read goes through [`Look`], so every rule is tested on paths of
//! every platform from any one of them.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use super::REPO;

/// How snob is installed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Method {
    /// Homebrew or Linuxbrew, through the tap this repository is: `brew` is
    /// the prefix's own, so a scheduler with a bare `PATH` finds it too.
    Homebrew { brew: PathBuf },
    /// Scoop, through the bucket this repository is: the root's `scoop.cmd`.
    Scoop { scoop: PathBuf },
    /// WinGet's portable install.
    Winget,
    /// `cargo install --git`: the `cargo` beside the binary.
    Cargo { cargo: PathBuf },
    /// The `.deb`, which only root can install.
    Deb,
    /// A copy in a directory this user writes, as `install.sh` and
    /// `install.ps1` leave it: snob replaces itself.
    Script,
    /// A build in a Cargo `target` directory: a developer's, never updated
    /// and never checked for.
    Dev,
    /// Anything else: told about, never touched.
    Unknown,
}

/// Who puts a new version in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applies {
    /// snob, over its own binary.
    Itself,
    /// The package manager, run by snob.
    Manager,
    /// The person, with a command snob prints (root's, for the `.deb`).
    ByHand,
    /// Nobody: a developer's build, or an install snob does not recognize.
    Never,
}

/// One program run to fetch or put in place a version, with its arguments
/// and what it is told in its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(&'static str, &'static str)>,
}

impl Step {
    fn of(program: impl Into<PathBuf>, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            env: Vec::new(),
        }
    }

    fn with_env(mut self, name: &'static str, value: &'static str) -> Self {
        self.env.push((name, value));
        self
    }
}

/// The formula's full name, so the tap's is the one upgraded whatever else
/// is called `snob`.
const FORMULA: &str = "dennisgr7/snob/snob";
/// WinGet's identifier for snob.
const WINGET_ID: &str = "dennisgr7.snob";

impl Method {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Method::Homebrew { .. } => "homebrew",
            Method::Scoop { .. } => "scoop",
            Method::Winget => "winget",
            Method::Cargo { .. } => "cargo",
            Method::Deb => "deb",
            Method::Script => "script",
            Method::Dev => "dev",
            Method::Unknown => "unknown",
        }
    }

    pub(crate) fn applies(&self) -> Applies {
        match self {
            Method::Script => Applies::Itself,
            Method::Homebrew { .. }
            | Method::Scoop { .. }
            | Method::Winget
            | Method::Cargo { .. } => Applies::Manager,
            Method::Deb => Applies::ByHand,
            Method::Dev | Method::Unknown => Applies::Never,
        }
    }

    /// What fetches `version` ahead of putting it in place, where the
    /// manager can: its bottle or archive lands in its own cache, and the
    /// upgrade later is quick. The `.deb` and a script install are downloaded
    /// by snob itself, not by a step.
    pub(crate) fn prefetch(&self) -> Vec<Step> {
        match self {
            // The formula is read from the tap, which only `brew update`
            // brings forward; the upgrade after it then need not update
            // again.
            Method::Homebrew { brew } => vec![
                Step::of(brew, &["update", "--quiet"]),
                Step::of(brew, &["fetch", "--quiet", "--formula", FORMULA]),
            ],
            Method::Scoop { scoop } => vec![
                Step::of(scoop, &["update"]),
                Step::of(scoop, &["download", "snob"]),
            ],
            _ => Vec::new(),
        }
    }

    /// What puts `version` in place, for a manager's install.
    pub(crate) fn apply(&self, version: &str) -> Vec<Step> {
        match self {
            Method::Homebrew { brew } => vec![
                Step::of(brew, &["upgrade", "--quiet", "--formula", FORMULA])
                    .with_env("HOMEBREW_NO_AUTO_UPDATE", "1"),
            ],
            Method::Scoop { scoop } => vec![Step::of(scoop, &["update", "snob"])],
            Method::Winget => vec![Step::of(
                "winget",
                &[
                    "upgrade",
                    "--id",
                    WINGET_ID,
                    "--exact",
                    "--silent",
                    "--disable-interactivity",
                    "--accept-source-agreements",
                    "--accept-package-agreements",
                ],
            )],
            Method::Cargo { cargo } => {
                let git = format!("https://github.com/{REPO}");
                let tag = format!("v{version}");
                vec![Step::of(
                    cargo,
                    &[
                        "install", "--locked", "--git", &git, "--tag", &tag, "snob-cli",
                    ],
                )]
            }
            Method::Deb | Method::Script | Method::Dev | Method::Unknown => Vec::new(),
        }
    }

    /// The command a person types to update this install, `deb` being the
    /// downloaded package when there is one.
    pub(crate) fn by_hand(&self, deb: Option<&Path>) -> Option<String> {
        Some(match self {
            Method::Homebrew { .. } => "brew upgrade snob".to_string(),
            Method::Scoop { .. } => "scoop update snob".to_string(),
            Method::Winget => format!("winget upgrade --id {WINGET_ID}"),
            Method::Cargo { .. } => {
                format!("cargo install --locked --git https://github.com/{REPO} snob-cli")
            }
            Method::Deb => match deb {
                Some(deb) => format!("sudo apt install {}", deb.display()),
                None => "snob update".to_string(),
            },
            Method::Script => "snob update".to_string(),
            Method::Dev | Method::Unknown => return None,
        })
    }
}

/// What detection reads of the machine.
pub(crate) trait Look {
    /// A file's text, `None` when it cannot be read.
    fn read(&self, path: &Path) -> Option<String>;
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
    /// Whether this user may create files in `dir`.
    fn writable(&self, dir: &Path) -> bool;
}

/// The machine itself.
pub(crate) struct System;

impl Look for System {
    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    #[cfg(unix)]
    fn writable(&self, dir: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let Ok(dir) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a NUL-terminated path that outlives the call.
        unsafe { libc::access(dir.as_ptr(), libc::W_OK) == 0 }
    }

    /// Windows: inside this user's profile. Asking the access list would be
    /// right and long; the install script puts snob under
    /// `%LOCALAPPDATA%\Programs`, and anything elsewhere is left to
    /// [`Method::Unknown`] rather than written to on a guess.
    #[cfg(windows)]
    fn writable(&self, dir: &Path) -> bool {
        ["LOCALAPPDATA", "USERPROFILE"]
            .into_iter()
            .filter_map(std::env::var_os)
            .any(|home| inside(dir, Path::new(&home)))
    }
}

/// Whether `path` is `dir` or inside it, compared without case, as Windows
/// compares names.
#[cfg_attr(not(windows), allow(dead_code))]
fn inside(path: &Path, dir: &Path) -> bool {
    let lower = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    let (path, dir) = (lower(path), lower(dir));
    let dir = dir.trim_end_matches('\\');
    path == dir || path.starts_with(&format!("{dir}\\"))
}

/// How the binary at `real` (every link followed) was installed.
pub(crate) fn detect(real: &Path, look: &dyn Look) -> Method {
    let names: Vec<String> = real
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let name = |i: usize| names.get(i).map(String::as_str);
    let n = names.len();
    let dir = real.parent().unwrap_or(real);

    // Before anything else: a developer's build in a directory they write
    // must never replace itself with a release.
    if a_build(real) {
        return Method::Dev;
    }

    // `<prefix>/Cellar/snob/<version>/bin/snob`.
    if n >= 5
        && name(n - 5) == Some("Cellar")
        && name(n - 4) == Some("snob")
        && name(n - 2) == Some("bin")
        && let Some(prefix) = ancestor(real, 5)
    {
        let brew = prefix.join("bin").join("brew");
        let brew = if look.exists(&brew) {
            brew
        } else {
            PathBuf::from("brew")
        };
        return Method::Homebrew { brew };
    }

    // `<root>/apps/snob/<version>/snob.exe`.
    if n >= 4
        && name(n - 4) == Some("apps")
        && name(n - 3) == Some("snob")
        && let Some(root) = ancestor(real, 4)
    {
        return Method::Scoop {
            scoop: root.join("shims").join("scoop.cmd"),
        };
    }

    // `...\WinGet\Packages\dennisgr7.snob_<source>\<archive>\snob.exe`.
    let winget_package = |i: usize| {
        name(i).is_some_and(|package| package.starts_with(&format!("{WINGET_ID}_")))
            && i > 0
            && name(i - 1) == Some("Packages")
    };
    if (1..n).any(winget_package) {
        return Method::Winget;
    }

    // `<root>/bin/snob`, `<root>` being Cargo's home or an `--root`: Cargo
    // keeps what it installed from where in `.crates2.json` beside `bin`.
    if dir.file_name().is_some_and(|bin| bin == "bin")
        && let Some(root) = dir.parent()
        && look
            .read(&root.join(".crates2.json"))
            .is_some_and(|installed| installed.contains("\"snob-cli "))
    {
        let cargo = dir.join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
        let cargo = if look.exists(&cargo) {
            cargo
        } else {
            PathBuf::from("cargo")
        };
        return Method::Cargo { cargo };
    }

    // The package's own list of its files names the binary.
    if look
        .read(Path::new("/var/lib/dpkg/info/snob.list"))
        .is_some_and(|files| files.lines().any(|line| Path::new(line) == real))
    {
        return Method::Deb;
    }

    if look.writable(dir) {
        Method::Script
    } else {
        Method::Unknown
    }
}

/// `path` with its last `levels` names taken off.
fn ancestor(path: &Path, levels: usize) -> Option<&Path> {
    (0..levels).try_fold(path, |path, _| path.parent())
}

/// The running binary's path, every link followed.
///
/// **On Linux, the path it was started from even once it was replaced**:
/// the kernel then names the running file `<path> (deleted)`, which is no
/// file, while the path itself now holds the new binary, which is the one
/// a question about this install is about.
pub(crate) fn real_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    #[cfg(target_os = "linux")]
    let exe = {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = exe.as_os_str().as_bytes();
        match bytes.strip_suffix(b" (deleted)") {
            Some(kept) if !exe.exists() => PathBuf::from(OsString::from_vec(kept.to_vec())),
            _ => exe,
        }
    };
    std::fs::canonicalize(exe)
}

/// How this binary was installed.
pub(crate) fn this_binary() -> Method {
    match real_exe() {
        Ok(real) => detect(&real, &System),
        Err(_) => Method::Unknown,
    }
}

/// Whether the binary at `real` is a build in a Cargo `target` directory:
/// `target/debug/snob`, `target/release/snob`, and the same under a target
/// triple.
pub(crate) fn a_build(real: &Path) -> bool {
    let names: Vec<_> = real.components().collect();
    let builds = |i: usize| {
        names
            .get(i)
            .is_some_and(|c| c.as_os_str() == "debug" || c.as_os_str() == "release")
    };
    (0..names.len()).any(|i| names[i].as_os_str() == "target" && (builds(i + 1) || builds(i + 2)))
}

/// Whether this binary is a developer's build, asked on every start; cheap,
/// since it reads nothing but the path. A binary that cannot say where it is
/// cannot be replaced either, and counts as one.
pub(crate) fn this_is_a_build() -> bool {
    real_exe().map_or(true, |real| a_build(&real))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// A machine made of the files and directories a test names.
    #[derive(Default)]
    struct Machine {
        files: HashMap<PathBuf, String>,
        writable: HashSet<PathBuf>,
    }

    impl Look for Machine {
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }
        fn writable(&self, dir: &Path) -> bool {
            self.writable.contains(dir)
        }
    }

    #[test]
    fn a_build_in_a_target_directory_is_a_developers() {
        let mut machine = Machine::default();
        for build in [
            "/home/me/snob-cli/target/debug/snob",
            "/home/me/snob-cli/target/release/snob",
            "/home/me/snob-cli/target/x86_64-unknown-linux-musl/release/snob",
        ] {
            // Even where its directory is writable, which it always is.
            machine
                .writable
                .insert(Path::new(build).parent().unwrap().into());
            assert_eq!(detect(Path::new(build), &machine), Method::Dev, "{build}");
        }
    }

    #[test]
    fn homebrew_runs_its_prefixs_brew() {
        let mut machine = Machine::default();
        machine
            .files
            .insert("/opt/homebrew/bin/brew".into(), String::new());
        let found = detect(
            Path::new("/opt/homebrew/Cellar/snob/0.9.0/bin/snob"),
            &machine,
        );
        assert_eq!(
            found,
            Method::Homebrew {
                brew: "/opt/homebrew/bin/brew".into()
            }
        );
        assert_eq!(found.applies(), Applies::Manager);
        assert_eq!(found.by_hand(None).as_deref(), Some("brew upgrade snob"));
        let upgrade = &found.apply("0.10.0")[0];
        assert_eq!(upgrade.env, [("HOMEBREW_NO_AUTO_UPDATE", "1")]);
        assert!(upgrade.args.iter().any(|a| a == FORMULA));
    }

    #[test]
    fn linuxbrew_is_homebrew_too() {
        let found = detect(
            Path::new("/home/linuxbrew/.linuxbrew/Cellar/snob/0.5.0/bin/snob"),
            &Machine::default(),
        );
        assert_eq!(
            found,
            Method::Homebrew {
                brew: "brew".into()
            }
        );
    }

    #[test]
    fn scoop_runs_its_roots_scoop() {
        let found = detect(
            Path::new(r"C:\Users\me\scoop\apps\snob\0.9.0\snob.exe"),
            &Machine::default(),
        );
        // Read as Unix paths on a Unix test machine, where a backslash is no
        // separator; the rule is checked on real separators below.
        if cfg!(windows) {
            assert_eq!(
                found,
                Method::Scoop {
                    scoop: r"C:\Users\me\scoop\shims\scoop.cmd".into()
                }
            );
        }
        let found = detect(
            Path::new("/c/Users/me/scoop/apps/snob/0.9.0/snob.exe"),
            &Machine::default(),
        );
        assert_eq!(
            found,
            Method::Scoop {
                scoop: "/c/Users/me/scoop/shims/scoop.cmd".into()
            }
        );
        assert_eq!(found.apply("0.10.0")[0].args, ["update", "snob"]);
    }

    #[test]
    fn winget_is_known_by_its_package_directory() {
        let found = detect(
            Path::new(
                "/c/Users/me/AppData/Local/Microsoft/WinGet/Packages/\
                 dennisgr7.snob_Microsoft.Winget.Source_8wekyb3d8bbwe/\
                 snob-v0.9.0-x86_64-pc-windows-msvc/snob.exe",
            ),
            &Machine::default(),
        );
        assert_eq!(found, Method::Winget);
        assert!(found.apply("0.10.0")[0].args.iter().any(|a| a == WINGET_ID));
    }

    #[test]
    fn cargo_is_known_by_its_record_of_what_it_installed() {
        let mut machine = Machine::default();
        machine.files.insert(
            "/home/me/.cargo/.crates2.json".into(),
            r#"{"installs":{"snob-cli 0.9.0 (git+https://github.com/dennisgr7/snob-cli#abc)":{}}}"#
                .into(),
        );
        let cargo = if cfg!(windows) {
            "/home/me/.cargo/bin/cargo.exe"
        } else {
            "/home/me/.cargo/bin/cargo"
        };
        machine.files.insert(cargo.into(), String::new());
        machine.writable.insert("/home/me/.cargo/bin".into());
        let found = detect(Path::new("/home/me/.cargo/bin/snob"), &machine);
        assert_eq!(
            found,
            Method::Cargo {
                cargo: cargo.into()
            }
        );
        let install = &found.apply("0.10.0")[0];
        assert!(install.args.iter().any(|a| a == "v0.10.0"), "{install:?}");
    }

    #[test]
    fn the_deb_is_known_by_its_list_of_files() {
        let mut machine = Machine::default();
        machine.files.insert(
            "/var/lib/dpkg/info/snob.list".into(),
            "/.\n/usr\n/usr/bin\n/usr/bin/snob\n".into(),
        );
        let found = detect(Path::new("/usr/bin/snob"), &machine);
        assert_eq!(found, Method::Deb);
        assert_eq!(found.applies(), Applies::ByHand);
        assert_eq!(
            found.by_hand(Some(Path::new("/tmp/snob.deb"))).as_deref(),
            Some("sudo apt install /tmp/snob.deb")
        );
    }

    /// What the install scripts leave: a copy in a directory this user
    /// writes, which snob replaces itself; one it cannot write is left alone.
    #[test]
    fn a_copy_snob_may_write_over_replaces_itself_and_any_other_is_left_alone() {
        let mut machine = Machine::default();
        machine.writable.insert("/home/me/.local/bin".into());
        let found = detect(Path::new("/home/me/.local/bin/snob"), &machine);
        assert_eq!(found, Method::Script);
        assert_eq!(found.applies(), Applies::Itself);

        let found = detect(Path::new("/usr/local/bin/snob"), &machine);
        assert_eq!(found, Method::Unknown);
        assert_eq!(found.applies(), Applies::Never);
        assert_eq!(found.by_hand(None), None);
    }

    #[test]
    fn a_path_inside_a_directory_is_compared_without_case() {
        assert!(inside(
            Path::new(r"C:\Users\Me\AppData\Local\Programs\snob"),
            Path::new(r"c:\users\me\appdata\local\")
        ));
        assert!(!inside(
            Path::new(r"C:\Users\Melanie\x"),
            Path::new(r"C:\Users\Me")
        ));
    }
}
