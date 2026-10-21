//! The home and data directories, found the conventional way for each
//! platform: `~/Library/Application Support` on macOS, the XDG base
//! directory spec elsewhere.

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use nix::unistd::{AccessFlags, User, access, geteuid};

/// `$HOME`, if it's set to an absolute path.
pub fn home_dir() -> Option<PathBuf> {
    home_from(std::env::var_os("HOME"))
}

/// Where applications keep their data. macOS ignores `XDG_DATA_HOME`, as
/// its native apps do.
pub fn data_dir() -> Option<PathBuf> {
    data_dir_from(
        cfg!(target_os = "macos"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_DATA_HOME"),
    )
}

/// A hint for when ferry can't keep its files in `$HOME` because it's
/// someone else's (`sudo -u` on macOS keeps the caller's `HOME`) or because
/// the account's own home isn't usable (service accounts often have
/// `/var/empty`). `None` when HOME looks fine, so the error lies elsewhere.
/// Lines after the first are indented to go under it.
pub fn home_hint() -> Option<String> {
    let me = geteuid();
    if me.is_root() {
        return None;
    }
    let home = home_dir()?;
    let user = User::from_uid(me).ok().flatten().map(|u| (u.name, u.dir));
    let own_home = match &user {
        Some((_, dir)) if writable(dir) => OwnHome::Usable,
        Some((_, dir)) if !dir.exists() => OwnHome::Missing,
        _ => OwnHome::Unwritable,
    };
    let facts = HomeFacts {
        home_writable: writable(&home),
        foreign: std::fs::metadata(&home).is_ok_and(|m| m.uid() != me.as_raw()),
        home,
        uid: me.as_raw(),
        user,
        own_home,
        macos: cfg!(target_os = "macos"),
    };
    Some(home_message(&facts)?.join("\n  "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnHome {
    Usable,
    Missing,
    Unwritable,
}

struct HomeFacts {
    /// `$HOME`.
    home: PathBuf,
    home_writable: bool,
    /// `$HOME` exists and belongs to another user.
    foreign: bool,
    uid: u32,
    /// The running user's name and home in the user database.
    user: Option<(String, PathBuf)>,
    own_home: OwnHome,
    macos: bool,
}

fn home_message(f: &HomeFacts) -> Option<Vec<String>> {
    if f.home_writable {
        return None;
    }
    let Some((name, own)) = &f.user else {
        // No account to give a home to, so set HOME instead.
        let uid = f.uid;
        let dir = "/var/lib/ferry";
        return Some(steps(
            format!(
                "ferry is running as uid {uid}, which isn't in the user database, \
                 so it has no home. To fix it:"
            ),
            &[
                (
                    "Make a directory for ferry".into(),
                    format!("sudo mkdir -p {dir} && sudo chown {uid} {dir}"),
                ),
                (
                    "Start ferry with HOME set".into(),
                    format!("sudo -u '#{uid}' env HOME={dir} ferry ..."),
                ),
            ],
        ));
    };
    let is_own = own == &f.home;
    if !is_own && !f.foreign {
        // HOME was pointed somewhere else on purpose.
        return None;
    }
    let (own, home) = (own.display(), f.home.display());
    let problem = match f.own_home {
        OwnHome::Usable if is_own => return None,
        OwnHome::Usable => {
            return Some(vec![
                format!(
                    "HOME is {home}, which belongs to another user, but ferry is running as {name}."
                ),
                format!(
                    "If you started it with sudo -u, add -H so HOME is {name}'s own home ({own})."
                ),
            ]);
        }
        OwnHome::Missing => "doesn't exist",
        OwnHome::Unwritable => "isn't writable",
    };
    let lead = if is_own {
        format!("ferry is running as {name}, whose home ({own}) {problem}. To fix it:")
    } else {
        format!(
            "HOME is {home}, which belongs to another user, and {name}'s own home \
             ({own}) {problem}. To fix it:"
        )
    };
    let dir = format!("/var/lib/{name}");
    let set_home = if f.macos {
        format!("sudo dscl . -create /Users/{name} NFSHomeDirectory {dir}")
    } else {
        format!("sudo usermod -d {dir} {name}")
    };
    Some(steps(
        lead,
        &[
            (
                format!("Make a home for {name}"),
                format!("sudo mkdir -p {dir} && sudo chown {name} {dir}"),
            ),
            (format!("Make it {name}'s home"), set_home),
            (
                "Start ferry with -H".into(),
                format!("sudo -H -u {name} ferry ..."),
            ),
        ],
    ))
}

/// `lead`, then numbered steps with their commands lined up.
fn steps(lead: String, steps: &[(String, String)]) -> Vec<String> {
    let width = steps
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    let mut lines = vec![lead];
    for (i, (label, command)) in steps.iter().enumerate() {
        let label = format!("{label}:");
        lines.push(format!(
            "  {}. {label:<w$}  {command}",
            i + 1,
            w = width + 1
        ));
    }
    lines
}

/// Whether this process can create files in `dir`.
fn writable(dir: &Path) -> bool {
    access(dir, AccessFlags::W_OK | AccessFlags::X_OK).is_ok()
}

fn home_from(home: Option<OsString>) -> Option<PathBuf> {
    absolute(home)
}

fn data_dir_from(
    macos: bool,
    home: Option<OsString>,
    xdg_data_home: Option<OsString>,
) -> Option<PathBuf> {
    if macos {
        return Some(home_from(home)?.join("Library/Application Support"));
    }
    // The spec says relative values are invalid and should be ignored.
    absolute(xdg_data_home).or_else(|| Some(home_from(home)?.join(".local/share")))
}

fn absolute(path: Option<OsString>) -> Option<PathBuf> {
    path.map(PathBuf::from).filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<OsString> {
        Some(v.into())
    }

    fn p(v: &str) -> Option<PathBuf> {
        Some(v.into())
    }

    #[test]
    fn home() {
        assert_eq!(home_from(s("/home/a")), p("/home/a"));
        assert_eq!(home_from(None), None);
        assert_eq!(home_from(s("")), None);
        assert_eq!(home_from(s("relative")), None);
    }

    fn facts(home: &str, foreign: bool, own_home: &str, state: OwnHome) -> HomeFacts {
        HomeFacts {
            home: home.into(),
            home_writable: false,
            foreign,
            uid: 260,
            user: Some(("_ferry".into(), own_home.into())),
            own_home: state,
            macos: true,
        }
    }

    fn hint(f: &HomeFacts) -> String {
        home_message(f).unwrap().join("\n  ")
    }

    #[test]
    fn home_hint_none_when_home_is_fine() {
        let mut f = facts("/Users/a", false, "/Users/a", OwnHome::Usable);
        assert_eq!(home_message(&f), None);
        // A writable HOME means the error is something else.
        f = facts("/Users/a", true, "/var/empty", OwnHome::Unwritable);
        f.home_writable = true;
        assert_eq!(home_message(&f), None);
        // HOME pointed somewhere of our own on purpose.
        f = facts("/srv/ferry", false, "/var/empty", OwnHome::Unwritable);
        assert_eq!(home_message(&f), None);
    }

    #[test]
    fn home_hint_sudo_kept_home() {
        let f = facts("/Users/a", true, "/var/lib/_ferry", OwnHome::Usable);
        assert_eq!(
            hint(&f),
            "HOME is /Users/a, which belongs to another user, but ferry is running as _ferry.\n  \
             If you started it with sudo -u, add -H so HOME is _ferry's own home (/var/lib/_ferry)."
        );
    }

    #[test]
    fn home_hint_own_home_unwritable() {
        let mut f = facts("/var/empty", true, "/var/empty", OwnHome::Unwritable);
        assert_eq!(
            hint(&f),
            "ferry is running as _ferry, whose home (/var/empty) isn't writable. To fix it:
    1. Make a home for _ferry:  sudo mkdir -p /var/lib/_ferry && sudo chown _ferry /var/lib/_ferry
    2. Make it _ferry's home:   sudo dscl . -create /Users/_ferry NFSHomeDirectory /var/lib/_ferry
    3. Start ferry with -H:     sudo -H -u _ferry ferry ..."
        );
        f.macos = false;
        assert!(
            hint(&f).contains("Make it _ferry's home:   sudo usermod -d /var/lib/_ferry _ferry\n")
        );
    }

    #[test]
    fn home_hint_own_home_missing() {
        let f = facts("/nonexistent", false, "/nonexistent", OwnHome::Missing);
        assert!(
            hint(&f).starts_with(
                "ferry is running as _ferry, whose home (/nonexistent) doesn't exist."
            )
        );
    }

    #[test]
    fn home_hint_sudo_kept_home_and_own_home_unwritable() {
        let f = facts("/Users/a", true, "/var/empty", OwnHome::Unwritable);
        let hint = hint(&f);
        assert!(hint.starts_with(
            "HOME is /Users/a, which belongs to another user, and _ferry's own home \
             (/var/empty) isn't writable. To fix it:\n"
        ));
        assert!(hint.contains("3. Start ferry with -H:     sudo -H -u _ferry ferry ..."));
    }

    #[test]
    fn home_hint_no_account() {
        let mut f = facts("/Users/a", true, "", OwnHome::Unwritable);
        f.user = None;
        assert_eq!(
            hint(&f),
            "ferry is running as uid 260, which isn't in the user database, so it has no home. To fix it:
    1. Make a directory for ferry:  sudo mkdir -p /var/lib/ferry && sudo chown 260 /var/lib/ferry
    2. Start ferry with HOME set:   sudo -u '#260' env HOME=/var/lib/ferry ferry ..."
        );
    }

    #[test]
    fn macos_data_dir_ignores_xdg() {
        let want = p("/Users/a/Library/Application Support");
        assert_eq!(data_dir_from(true, s("/Users/a"), None), want);
        assert_eq!(data_dir_from(true, s("/Users/a"), s("/xdg")), want);
        assert_eq!(data_dir_from(true, None, s("/xdg")), None);
    }

    #[test]
    fn xdg_data_dir() {
        assert_eq!(data_dir_from(false, s("/home/a"), s("/xdg")), p("/xdg"));
        assert_eq!(data_dir_from(false, None, s("/xdg")), p("/xdg"));
        let default = p("/home/a/.local/share");
        assert_eq!(data_dir_from(false, s("/home/a"), None), default);
        assert_eq!(data_dir_from(false, s("/home/a"), s("")), default);
        assert_eq!(data_dir_from(false, s("/home/a"), s("rel/dir")), default);
        assert_eq!(data_dir_from(false, None, None), None);
    }
}
