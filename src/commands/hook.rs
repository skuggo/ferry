//! `ferry hook` — Claude Code / Codex PreToolUse hook.
//!
//! Reads a hook-envelope JSON object from stdin, extracts the file path
//! from the tool's inputs, and — if the file lives under a ferry
//! project — pulls it (with a configurable cooldown). Always exits 0
//! so the LLM's tool call is never blocked; failures are surfaced on
//! stderr, which the hosting agent shows to the user.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use std::io::Read;
use std::path::Path;

use crate::commands::file_transfer::TransferStatus;
use crate::commands::{ExecutionMode, state_path_for};
use crate::state::StateFile;

/// Shape shared by Claude Code (snake_case) hook input. Codex uses a
/// slightly different envelope but the same `tool_name` / `tool_input`
/// nesting works. Missing fields are tolerated.
#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

pub fn run(cooldown_secs: i64, mode: ExecutionMode) -> Result<()> {
    let mut buf = String::new();
    if let Err(error) = std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading hook envelope on stdin")
    {
        eprintln!("ferry hook: {error:#}");
        return Ok(());
    }
    if buf.trim().is_empty() {
        return Ok(());
    }
    let input: HookInput = match serde_json::from_str(&buf).context("parsing hook envelope") {
        Ok(input) => input,
        Err(error) => {
            eprintln!("ferry hook: {error:#}");
            return Ok(());
        }
    };

    // Only some tools carry a file_path. Bash/Grep/Glob don't; allow them
    // through without any FTP work.
    let tool_input = match input.tool_input {
        Some(v) => v,
        None => return Ok(()),
    };
    let file_path = tool_input
        .get("file_path")
        .and_then(|v| v.as_str())
        .or_else(|| tool_input.get("path").and_then(|v| v.as_str()));
    let file_path = match file_path {
        Some(p) => Path::new(p),
        None => return Ok(()),
    };
    if !file_path.is_absolute() {
        // Claude Code sends absolute paths; anything else is likely a
        // shape we don't recognize — bail out silently rather than guess.
        return Ok(());
    }

    let resolved = match crate::project::resolve_file(file_path, mode.should_apply()) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!(
                "ferry hook: resolving {} failed: {error:#}",
                file_path.display()
            );
            return Ok(());
        }
    };
    let config_path = resolved.config_path;
    let state_path = state_path_for(&resolved.config.paths.local_root, mode);
    let rel = resolved.relative_path;

    // Hand the request to the project's helper, which keeps a session logged
    // in and the state in memory (see hookd.rs), and checks the cooldown
    // itself. With no helper answering, start one for next time and do the
    // work here the usual way. Dry runs never involve the helper.
    let tool = input.tool_name.as_deref().unwrap_or("<unknown>");
    if mode.should_apply() {
        let local_root = &resolved.config.paths.local_root;
        match crate::commands::hookd::request(local_root, &rel, true, cooldown_secs) {
            Some(crate::commands::hookd::Answer::Cooled) => {
                eprintln!("ferry hook: {tool} {rel} — within {cooldown_secs}s cooldown, skipping pull");
                return Ok(());
            }
            Some(crate::commands::hookd::Answer::Done(msg)) => {
                if !msg.is_empty() {
                    eprintln!("ferry hook: {msg}");
                }
                return Ok(());
            }
            None => crate::commands::hookd::spawn(&config_path),
        }
    }

    // Cooldown check. If the state entry's last_synced is within the
    // cooldown window, skip the pull. Apply mode keeps config loading deferred
    // until we know we're going to act; dry-run loaded it above only to resolve
    // the configured local root for read-through state selection.
    // The state loaded here is handed on to the pull rather than parsed
    // again: on a large mirror each load is ~10 ms of every hook call.
    let state = StateFile::load_or_default(&state_path).ok();
    if let Some(record) = state.as_ref().and_then(|state| state.files.get(&rel)) {
        let elapsed = Utc::now().signed_duration_since(record.last_synced);
        if elapsed.num_seconds() >= 0 && elapsed.num_seconds() < cooldown_secs {
            eprintln!("ferry hook: {tool} {rel} — within {cooldown_secs}s cooldown, skipping pull");
            return Ok(());
        }
    }

    // Fast single-file pull. The hook contract is "give me the current remote
    // version" -- but that stops short of discarding work the user has not
    // pushed yet.
    //
    // This used to pass force=true, which made the pull overwrite un-pushed
    // local edits with no warning and no way to get them back. It is a real
    // data-loss path, not staleness: an agent that edits a file twice without
    // pushing in between has its first edit silently reverted by the hook
    // firing before the second, and the only sign is that the change is
    // simply not there afterwards. Reproduced as:
    //
    //     echo MARKER >> NEWS                       # un-pushed local edit
    //     echo '{...NEWS...}' | ferry hook -v       # "ferry hook: pulled NEWS"
    //     grep -c MARKER NEWS                       # 0
    //
    // force=false makes pull_one apply the same BothChanged rule the `pull`
    // subcommand already applies, so a diverged file is reported instead of
    // clobbered. Recovering staleness costs an explicit `ferry pull`;
    // recovering a clobbered edit may cost the edit.
    let pulled = match state {
        Some(state) => crate::commands::pull::pull_one_with_state(
            &config_path,
            &rel,
            /* force = */ false,
            mode,
            state,
        ),
        // Unreadable here: let the pull load it and report why.
        None => crate::commands::pull::pull_one(&config_path, &rel, /* force = */ false, mode),
    };
    match pulled {
        Ok(outcome) if outcome.status == TransferStatus::Transferred => {
            if mode.is_dry_run() {
                eprintln!("ferry hook: would pull {}", outcome.path);
            } else {
                eprintln!("ferry hook: pulled {}", outcome.path);
            }
        }
        Ok(_) => {}
        Err(error) => {
            // Don't fail the hook — deny would surprise the user. Just log.
            // The common case here is a conflict: local has edits that were
            // never pushed, so the pull is refused and the local copy stands.
            eprintln!(
                "ferry hook: not pulling {rel} ({error:#}) -- \
                 local copy kept; run 'ferry push {rel}' or an explicit \
                 'ferry pull {rel}' to reconcile"
            );
            // stderr from an exit-0 PreToolUse hook never reaches the agent,
            // so on its own the tool call goes ahead on a possibly stale copy
            // with no sign anything was wrong. additionalContext does.
            println!("{}", agent_notice(&rel, &error));
        }
    }
    Ok(())
}

/// Claude Code PreToolUse output carrying `error` into the agent's context.
fn agent_notice(rel: &str, error: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": format!(
                "ferry did NOT refresh {rel} from the server, so the local copy \
                 you are about to use may not match it: {error:#}"
            ),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_input_parses_claude_code_shape() {
        let json = r#"{
            "session_id": "abc",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/foo.c"}
        }"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
        assert_eq!(
            input
                .tool_input
                .unwrap()
                .get("file_path")
                .and_then(|v| v.as_str()),
            Some("/tmp/foo.c")
        );
    }

    #[test]
    fn hook_input_tolerates_missing_fields() {
        let input: HookInput = serde_json::from_str("{}").unwrap();
        assert!(input.tool_name.is_none());
        assert!(input.tool_input.is_none());
    }
}
