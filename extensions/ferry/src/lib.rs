use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct FerryExtension;

impl zed::Extension for FerryExtension {
    fn new() -> Self {
        Self
    }

    // Zed calls this once per worktree the first time a C or C++ file is opened.
    // We just return the command to launch `ferry-lsp`; the LSP process
    // then handles textDocument/didOpen for every subsequent open in that
    // worktree.
    //
    // Prerequisite: the `ferry-lsp` binary must be on the user's PATH
    // (typically installed via `cargo install --path .` from the main
    // ferry repo). If it's not found, Zed shows a diagnostic and
    // auto-pull silently no-ops — the editor still works normally.
    fn language_server_command(
        &mut self,
        _lsp_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        command_for_path(worktree.which("ferry-lsp"))
    }
}

fn command_for_path(path: Option<String>) -> Result<Command> {
    let path = path.ok_or(
        "ferry-lsp was not found. Run `cargo install --path .` and ensure `ferry-lsp` is on the worktree shell PATH.",
    )?;

    Ok(Command {
        command: path,
        args: vec![],
        env: vec![],
    })
}

zed::register_extension!(FerryExtension);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_uses_discovered_absolute_path() {
        let command = command_for_path(Some("/home/test/.cargo/bin/ferry-lsp".into())).unwrap();

        assert_eq!(command.command, "/home/test/.cargo/bin/ferry-lsp");
        assert!(command.args.is_empty());
        assert!(command.env.is_empty());
    }

    #[test]
    fn missing_binary_returns_installation_hint() {
        let error = command_for_path(None).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("ferry-lsp"));
        assert!(message.contains("cargo install --path ."));
        assert!(message.contains("PATH"));
    }
}
