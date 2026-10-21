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
/// the account's own home isn't writable (service accounts often have
/// `/var/empty`). `None` when HOME looks fine, so the error lies elsewhere.
pub fn home_hint() -> Option<String> {
    let me = geteuid();
    if me.is_root() {
        return None;
    }
    let home = home_dir()?;
    let user = User::from_uid(me).ok().flatten();
    let own_home = user.as_ref().map(|u| u.dir.clone());
    let facts = HomeFacts {
        foreign: std::fs::metadata(&home).is_ok_and(|m| m.uid() != me.as_raw()),
        own_home_usable: own_home.as_deref().is_some_and(writable),
        own_home,
        name: user.map_or_else(|| me.to_string(), |u| u.name),
        home,
        macos: cfg!(target_os = "macos"),
    };
    home_message(&facts)
}

struct HomeFacts {
    /// `$HOME`.
    home: PathBuf,
    /// `$HOME` exists and belongs to another user.
    foreign: bool,
    /// The running user's name, or their uid if they have no name.
    name: String,
    /// The running user's home directory in the user database.
    own_home: Option<PathBuf>,
    own_home_usable: bool,
    macos: bool,
}

fn home_message(f: &HomeFacts) -> Option<String> {
    let name = &f.name;
    let is_own = f.own_home.as_deref() == Some(f.home.as_path());
    if !is_own && f.foreign {
        let mut msg = format!(
            "HOME is {}, which belongs to another user, but ferry is running as {name}. \
             If you started it with sudo -u, add -H so HOME is {name}'s own home.",
            f.home.display()
        );
        if !f.own_home_usable {
            msg += &format!(" That won't work yet either: {}", give_home(f));
        }
        return Some(msg);
    }
    if (is_own || f.own_home.is_none()) && !f.own_home_usable {
        return Some(format!(
            "{name} has no home directory it can write to. {}",
            give_home(f)
        ));
    }
    None
}

/// Commands that give the running user a home of its own.
fn give_home(f: &HomeFacts) -> String {
    let name = &f.name;
    let current = match &f.own_home {
        Some(dir) => format!(
            "{name}'s home is {}, which it can't write to",
            dir.display()
        ),
        None => format!("{name} has no home directory"),
    };
    let dir = format!("/var/lib/{name}");
    let set_home = if f.macos {
        format!("sudo dscl . -create /Users/{name} NFSHomeDirectory {dir}")
    } else {
        format!("sudo usermod -d {dir} {name}")
    };
    format!(
        "{current}. To give it one, run:\n  \
         sudo mkdir -p {dir} && sudo chown {name} {dir}\n  \
         {set_home}\n\
         then start ferry with sudo -H -u {name}."
    )
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

    fn facts(home: &str, foreign: bool, own_home: Option<&str>, usable: bool) -> HomeFacts {
        HomeFacts {
            home: home.into(),
            foreign,
            name: "_ferry".into(),
            own_home: own_home.map(PathBuf::from),
            own_home_usable: usable,
            macos: true,
        }
    }

    #[test]
    fn home_hint_none_when_home_is_fine() {
        let f = facts("/Users/a", false, Some("/Users/a"), true);
        assert_eq!(home_message(&f), None);
        // HOME pointed somewhere of our own on purpose.
        let f = facts("/srv/ferry", false, Some("/var/empty"), false);
        assert_eq!(home_message(&f), None);
    }

    #[test]
    fn home_hint_sudo_kept_home() {
        let f = facts("/Users/a", true, Some("/var/lib/_ferry"), true);
        let hint = home_message(&f).unwrap();
        assert!(hint.starts_with("HOME is /Users/a, which belongs to another user"));
        assert!(hint.contains("running as _ferry"));
        assert!(hint.contains("add -H"));
        assert!(!hint.contains("mkdir"));
    }

    #[test]
    fn home_hint_sudo_kept_home_and_own_home_unusable() {
        let f = facts("/Users/a", true, Some("/var/empty"), false);
        let hint = home_message(&f).unwrap();
        assert!(hint.contains("add -H"));
        assert!(hint.contains("_ferry's home is /var/empty, which it can't write to"));
        assert!(
            hint.contains("sudo mkdir -p /var/lib/_ferry && sudo chown _ferry /var/lib/_ferry")
        );
        assert!(
            hint.contains("sudo dscl . -create /Users/_ferry NFSHomeDirectory /var/lib/_ferry")
        );
    }

    #[test]
    fn home_hint_own_home_unusable() {
        let mut f = facts("/var/empty", true, Some("/var/empty"), false);
        let hint = home_message(&f).unwrap();
        assert!(hint.starts_with("_ferry has no home directory it can write to"));
        assert!(!hint.contains("add -H"));
        assert!(hint.contains("then start ferry with sudo -H -u _ferry"));
        f.macos = false;
        let hint = home_message(&f).unwrap();
        assert!(hint.contains("sudo usermod -d /var/lib/_ferry _ferry"));
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
