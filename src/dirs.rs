//! The home and data directories, found the conventional way for each
//! platform: `~/Library/Application Support` on macOS, the XDG base
//! directory spec elsewhere.

use std::ffi::OsString;
use std::path::PathBuf;

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
