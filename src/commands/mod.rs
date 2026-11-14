#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Apply,
    DryRun,
}

impl ExecutionMode {
    pub fn from_dry_run(dry_run: bool) -> Self {
        if dry_run { Self::DryRun } else { Self::Apply }
    }

    pub fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }

    pub fn should_apply(self) -> bool {
        matches!(self, Self::Apply)
    }
}

/// Select the state file used by a command. Prefer the current state file, but
/// preserve a legacy state file as the target until migration can complete.
pub fn state_path_for(local_root: &std::path::Path, _mode: ExecutionMode) -> std::path::PathBuf {
    let current = local_root.join(crate::names::STATE_DIR).join("state.json");
    if current.exists() {
        return current;
    }

    let legacy = local_root
        .join(crate::names::LEGACY_STATE_DIR)
        .join("state.json");
    if legacy.exists() { legacy } else { current }
}

#[cfg(test)]
mod execution_mode_tests {
    use super::{ExecutionMode, state_path_for};

    #[test]
    fn state_path_reads_legacy_for_both_modes_as_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir
            .path()
            .join(crate::names::LEGACY_STATE_DIR)
            .join("state.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "legacy").unwrap();

        assert_eq!(state_path_for(dir.path(), ExecutionMode::DryRun), legacy);
        assert_eq!(state_path_for(dir.path(), ExecutionMode::Apply), legacy);
    }

    #[test]
    fn dry_run_state_path_prefers_current_over_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(crate::names::STATE_DIR).join("state.json");
        let legacy = dir
            .path()
            .join(crate::names::LEGACY_STATE_DIR)
            .join("state.json");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&current, "current").unwrap();
        std::fs::write(&legacy, "legacy").unwrap();

        assert_eq!(state_path_for(dir.path(), ExecutionMode::DryRun), current);
    }

    #[test]
    fn maps_cli_flag_and_reports_write_permission() {
        assert_eq!(ExecutionMode::from_dry_run(false), ExecutionMode::Apply);
        assert_eq!(ExecutionMode::from_dry_run(true), ExecutionMode::DryRun);
        assert!(ExecutionMode::Apply.should_apply());
        assert!(!ExecutionMode::DryRun.should_apply());
    }

    #[test]
    fn apply_state_path_reads_legacy_when_current_state_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let current_dir = dir.path().join(crate::names::STATE_DIR);
        let legacy = dir
            .path()
            .join(crate::names::LEGACY_STATE_DIR)
            .join("state.json");
        std::fs::create_dir(&current_dir).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "legacy").unwrap();

        assert_eq!(state_path_for(dir.path(), ExecutionMode::Apply), legacy);
    }
}

pub mod cc;
pub mod clean;
pub mod file_transfer;
pub mod hook;
pub mod hookd;
pub mod init;
pub mod ls;
pub mod prune;
pub mod pull;
pub mod push;
pub mod remote_hash;
pub mod rm;
pub mod status;
pub mod sync;
pub(crate) mod transfer_temp;
pub mod walk;
