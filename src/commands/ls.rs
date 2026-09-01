//! `ferry ls [PATH]` — minimal remote listing for connectivity smoke tests.
//!
//! Reads only `[connection]` + `[paths]` from `.ferry.toml`; doesn't touch
//! state, doesn't walk local. PATH is optional: empty means list the
//! configured `remote_root`.

use crate::config::Config;
use crate::ftp::{Entry, Ftp};
use anyhow::Result;
use std::path::Path;

pub fn run(config_path: &Path, sub: Option<&str>) -> Result<()> {
    let cfg = Config::load(config_path)?;

    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;

    let dir = match sub {
        None | Some("") => cfg.paths.remote_root.clone(),
        Some(p) => {
            let root = cfg.paths.remote_root.trim_end_matches('/');
            let p = p.trim_start_matches('/');
            if p.is_empty() {
                cfg.paths.remote_root.clone()
            } else {
                format!("{root}/{p}")
            }
        }
    };

    let entries = ftp.list(&dir)?;
    for e in entries {
        let kind = kind_char(&e);
        println!(
            "{kind} {size:>10} {mtime}  {name}",
            kind = kind,
            size = e.size,
            mtime = e.modified.format("%Y-%m-%d %H:%M:%S"),
            name = e.name,
        );
    }
    Ok(())
}

/// The leading type character, mirroring `ls -l`. Symlinks get their own
/// marker: `ls` is the one command that still shows them, because every
/// syncing command skips them and every write path refuses them, and a user
/// staring at a "missing" file needs to be able to see why.
fn kind_char(entry: &Entry) -> char {
    if entry.is_symlink {
        'l'
    } else if entry.is_dir {
        'd'
    } else {
        '-'
    }
}

#[cfg(test)]
mod tests {
    use super::kind_char;
    use crate::ftp::Entry;
    use chrono::Utc;

    fn entry(is_dir: bool, is_symlink: bool) -> Entry {
        Entry {
            name: "x".into(),
            is_dir,
            is_symlink,
            size: 0,
            modified: Utc::now(),
        }
    }

    #[test]
    fn a_symlink_is_marked_even_though_it_is_not_a_directory() {
        assert_eq!(kind_char(&entry(false, true)), 'l');
    }

    #[test]
    fn directories_and_files_keep_their_markers() {
        assert_eq!(kind_char(&entry(true, false)), 'd');
        assert_eq!(kind_char(&entry(false, false)), '-');
    }
}
