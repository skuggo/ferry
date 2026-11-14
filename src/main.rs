use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ferry",
    version,
    about = "FTP sync helper for editors and coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,
    #[arg(long, short, global = true)]
    verbose: bool,
    /// Preview write-capable commands without changing local, remote, or Ferry state.
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive setup; validates existing local files vs remote.
    Init {
        #[arg(long)]
        no_validate: bool,
    },
    /// List a remote directory (connectivity smoke test). PATH is relative
    /// to `paths.remote_root` from the config; empty lists the root itself.
    Ls { path: Option<String> },
    /// Claude Code / Codex PreToolUse hook. Reads a hook envelope on
    /// stdin and pulls the referenced file with a cooldown window.
    /// Always exits 0 so the LLM tool call is never blocked.
    Hook {
        /// Skip the pull if the file was pulled within this many seconds.
        #[arg(long, default_value_t = 3600)]
        cooldown: i64,
    },
    /// Keeps one FTP session logged in for `ferry hook` (started by the hook
    /// itself; exits after 15 idle minutes).
    #[command(hide = true)]
    Hookd,
    /// Show per-file sync state vs remote. PATHS (files or directories,
    /// relative to the current directory) limit the report; with none, a
    /// subdirectory of the project reports just that subtree.
    Status { paths: Vec<String> },
    /// Download remote -> local.
    Pull {
        paths: Vec<String>,
        #[arg(long)]
        force: bool,
        /// Also delete local files the remote no longer has: only untouched
        /// copies of synced files, and only once a listing proves them gone.
        /// Needs explicit paths.
        #[arg(long)]
        prune: bool,
    },
    /// Upload local -> remote.
    Push {
        paths: Vec<String>,
        #[arg(long)]
        force: bool,
        /// Also delete remote files that were deleted locally: only untouched
        /// copies of synced files. Needs explicit paths that exist locally.
        #[arg(long)]
        prune: bool,
    },
    /// Pull then push; refuses on conflict unless --force.
    Sync {
        path: Option<String>,
        #[arg(long, conflicts_with = "path")]
        select: bool,
        #[arg(long)]
        force: bool,
    },
    /// Delete files on the remote server and the local mirror (and drop their
    /// state records). Requires explicit paths; pass --recursive to delete a
    /// directory subtree.
    Rm {
        paths: Vec<String>,
        #[arg(long)]
        recursive: bool,
    },
    /// Remove ferry's own leftover upload temps (`ferry-tmp.<hex>`) under the
    /// given remote paths, if older than --hours. Nothing else is touched.
    Clean {
        paths: Vec<String>,
        #[arg(long, default_value_t = ferry::commands::clean::DEFAULT_MIN_AGE_HOURS)]
        hours: i64,
    },
    /// Check-compile files on the MUD via the UDP compile service. Prints
    /// per-file OK/FAIL and diagnostics; exits non-zero if any failed.
    #[command(alias = "check")]
    Cc { paths: Vec<String> },
}

/// Exit codes (consumed by Zed's `tasks.json`):
/// - 0 success
/// - 1 generic error (default for anything we don't classify)
/// - 2 conflict — refused without `--force`
/// - 3 config or auth problem — Zed should prompt to fix `.ferry.toml`
///   rather than retry blindly
fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let cli = Cli::parse();
    if matches!(&cli.cmd, Cmd::Sync { select: true, .. })
        && let Err(error) = ferry::commands::sync::require_select_terminal()
    {
        return finish(Err(error));
    }
    let mode = ferry::commands::ExecutionMode::from_dry_run(cli.dry_run);
    if matches!(&cli.cmd, Cmd::Rm { paths, .. } if paths.is_empty()) {
        return finish(ferry::commands::rm::run(
            std::path::Path::new(""),
            &[],
            false,
            mode,
        ));
    }
    let explicit_config = cli.config.is_some();
    let mut cfg = cli.config.unwrap_or_else(|| default_config_path(&cli.cmd));
    let explicit_legacy_config = explicit_config
        && cfg
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new(ferry::names::LEGACY_CONFIG_FILE));
    let explicit_legacy_entry_exists = explicit_legacy_config && path_entry_exists(&cfg);
    let should_load_config = !matches!(&cli.cmd, Cmd::Init { .. })
        && !matches!(&cli.cmd, Cmd::Rm { paths, .. } if paths.is_empty());
    if should_prepare_config_before_dispatch(&cli.cmd, mode) {
        let config_dir = cfg.parent().unwrap_or_else(|| std::path::Path::new("."));
        if let Err(e) = ferry::names::migrate_legacy(config_dir) {
            eprintln!("warning: {e:#}");
        }
        if !explicit_config || (explicit_legacy_entry_exists && path_entry_is_missing(&cfg)) {
            cfg = ferry::names::config_path_for_read(config_dir);
        }

        if should_load_config {
            match ferry::config::Config::load(&cfg) {
                Ok(config) => {
                    if let Err(e) = ferry::names::migrate_legacy(&config.paths.local_root) {
                        eprintln!("warning: {e:#}");
                    }
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    return classify_exit(&e);
                }
            }
        }
    }
    let result: anyhow::Result<()> = (|| {
        // Like git: relative paths are taken from the working directory, and
        // a command run in a subdirectory with no paths covers that subtree.
        let offset = cwd_offset_for(&cli.cmd, &cfg);
        let rebase = |paths: &[String], default_to_cwd| {
            ferry::project::rebase_cli_paths(offset.as_deref(), paths, default_to_cwd)
        };
        let rebase_one = |path: Option<String>| -> anyhow::Result<Option<String>> {
            Ok(rebase(&Vec::from_iter(path), true)?.into_iter().next())
        };
        match cli.cmd {
            Cmd::Init { no_validate } => ferry::commands::init::run(&cfg, no_validate, mode),
            Cmd::Ls { path } => ferry::commands::ls::run(&cfg, rebase_one(path)?.as_deref()),
            Cmd::Hook { cooldown } => ferry::commands::hook::run(cooldown, mode),
            Cmd::Hookd => ferry::commands::hookd::run(&cfg),
            Cmd::Status { paths } => {
                ferry::commands::status::run(&cfg, &rebase(&paths, true)?, mode)
            }
            Cmd::Pull { paths, force, prune } => {
                ferry::commands::pull::run(&cfg, &rebase(&paths, true)?, force, prune, mode)
            }
            Cmd::Push { paths, force, prune } => {
                ferry::commands::push::run(&cfg, &rebase(&paths, true)?, force, prune, mode)
            }
            Cmd::Sync {
                path,
                select,
                force,
            } => {
                let path = if select { path } else { rebase_one(path)? };
                ferry::commands::sync::run_cli(&cfg, path.as_deref(), select, force, mode)
            }
            // Never defaulted: a bare `rm` in a subdirectory must not delete it.
            Cmd::Rm { paths, recursive } => {
                ferry::commands::rm::run(&cfg, &rebase(&paths, false)?, recursive, mode)
            }
            Cmd::Cc { paths } => ferry::commands::cc::run(&cfg, &rebase(&paths, false)?),
            Cmd::Clean { paths, hours } => {
                ferry::commands::clean::run(&cfg, &rebase(&paths, false)?, hours, mode)
            }
        }
    })();
    finish(result)
}

/// Where the working directory sits inside the project `cfg` describes, for
/// commands that take project paths. `None` (no rebasing) when the config
/// cannot be read -- the command reports that itself -- or the working
/// directory is outside `local_root`.
fn cwd_offset_for(cmd: &Cmd, cfg: &std::path::Path) -> Option<String> {
    if matches!(cmd, Cmd::Init { .. } | Cmd::Hook { .. } | Cmd::Hookd) {
        return None;
    }
    let config = ferry::config::Config::load(cfg).ok()?;
    let cwd = std::env::current_dir().ok()?;
    ferry::project::cwd_offset(&config.paths.local_root, &cwd)
}

fn finish(result: anyhow::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            // `{:#}` expands the full anyhow context chain on one line so
            // users see e.g. "config: reading .ferry.toml: No such file…"
            // rather than just the outermost message.
            eprintln!("error: {e:#}");
            classify_exit(&e)
        }
    }
}

fn path_entry_exists(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn path_entry_is_missing(path: &std::path::Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn should_prepare_config_before_dispatch(cmd: &Cmd, mode: ferry::commands::ExecutionMode) -> bool {
    mode.should_apply() && !matches!(cmd, Cmd::Hook { .. } | Cmd::Sync { select: true, .. })
}

fn default_config_path(cmd: &Cmd) -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    default_config_path_at(cmd, &cwd)
}

fn default_config_path_at(cmd: &Cmd, cwd: &std::path::Path) -> std::path::PathBuf {
    if matches!(cmd, Cmd::Init { .. }) {
        return std::path::PathBuf::from(ferry::names::CONFIG_FILE);
    }
    ferry::project::find_config_upward(cwd)
        .map(|location| location.config_path)
        .unwrap_or_else(|| ferry::names::config_path_for_read(cwd))
}

/// Map an anyhow error onto a process exit code. We check both the root
/// error and the entire `.chain()` so an `Exit::*` wrapped by a later
/// `.with_context(...)` still resolves to its specific exit code.
fn classify_exit(e: &anyhow::Error) -> i32 {
    use ferry::Exit;
    // anyhow's downcast_ref looks at the root; if the Exit was wrapped by
    // `.context(...)` later it won't match here, hence the chain walk below.
    if let Some(exit) = e.downcast_ref::<Exit>() {
        return code_for(exit);
    }
    for cause in e.chain() {
        if let Some(exit) = cause.downcast_ref::<Exit>() {
            return code_for(exit);
        }
    }
    1
}

fn code_for(exit: &ferry::Exit) -> i32 {
    use ferry::Exit;
    match exit {
        Exit::Conflict(_) => 2,
        Exit::Config(_) | Exit::Auth(_) => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_accepts_zero_or_one_path_or_select() {
        assert!(matches!(
            Cli::try_parse_from(["ferry", "sync"]).unwrap().cmd,
            Cmd::Sync {
                path: None,
                select: false,
                force: false
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["ferry", "sync", "areas"])
                .unwrap()
                .cmd,
            Cmd::Sync {
                path: Some(path),
                select: false,
                force: false
            } if path == "areas"
        ));
        assert!(matches!(
            Cli::try_parse_from(["ferry", "sync", "--select"])
                .unwrap()
                .cmd,
            Cmd::Sync {
                path: None,
                select: true,
                force: false
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["ferry", "sync", "areas", "--force"])
                .unwrap()
                .cmd,
            Cmd::Sync {
                path: Some(path),
                select: false,
                force: true
            } if path == "areas"
        ));
        assert!(matches!(
            Cli::try_parse_from(["ferry", "sync", "--select", "--force"])
                .unwrap()
                .cmd,
            Cmd::Sync {
                path: None,
                select: true,
                force: true
            }
        ));
    }

    #[test]
    fn sync_rejects_multiple_paths_and_path_plus_select() {
        assert!(Cli::try_parse_from(["ferry", "sync", "one", "two"]).is_err());
        assert!(Cli::try_parse_from(["ferry", "sync", "one", "--select"]).is_err());
    }

    #[test]
    fn init_default_config_stays_in_cwd() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(project.path().join(ferry::names::CONFIG_FILE), "ancestor").unwrap();

        let init = Cmd::Init { no_validate: true };
        assert_eq!(
            default_config_path_at(&init, &nested),
            std::path::PathBuf::from(ferry::names::CONFIG_FILE),
        );
    }
}

#[cfg(test)]
mod select_dispatch_tests {
    use super::{Cmd, default_config_path_at, should_prepare_config_before_dispatch};
    use ferry::commands::ExecutionMode;

    #[test]
    fn select_skips_preselection_migration_so_cancel_stays_read_only() {
        let select = Cmd::Sync {
            path: None,
            select: true,
            force: false,
        };
        let direct = Cmd::Sync {
            path: Some("areas".into()),
            select: false,
            force: false,
        };
        let bare = Cmd::Sync {
            path: None,
            select: false,
            force: false,
        };

        assert!(!should_prepare_config_before_dispatch(
            &select,
            ExecutionMode::Apply
        ));
        assert!(should_prepare_config_before_dispatch(
            &direct,
            ExecutionMode::Apply
        ));
        assert!(should_prepare_config_before_dispatch(
            &bare,
            ExecutionMode::Apply
        ));
    }

    #[test]
    fn select_discovers_the_nearest_config_from_the_task_cwd() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("one/two");
        std::fs::create_dir_all(&nested).unwrap();
        let config = project.path().join(ferry::names::CONFIG_FILE);
        std::fs::write(&config, "config").unwrap();
        let select = Cmd::Sync {
            path: None,
            select: true,
            force: false,
        };

        assert_eq!(default_config_path_at(&select, &nested), config);
    }
}
