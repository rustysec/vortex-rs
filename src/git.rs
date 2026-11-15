//! Git lookups for the status row.
//!
//! The branch name is read by asking `git` itself — vortex does not parse
//! `.git` internals.  The probe runs on the async runtime so a slow (or cold,
//! networked) repository never stalls a frame; whoever asks gets whatever the
//! last finished probe found.

use std::path::Path;

use tokio::process::Command;

use crate::event::{AsyncEvent, EventTx};

/// Ask `git` for the branch of the repository containing `root`, reporting
/// back through `tx`.  A detached head shows as `@<short-sha>`; anything that
/// is not a repository reports `None` and draws no segment.
pub fn probe_branch(tx: EventTx, root: std::path::PathBuf) {
    tokio::spawn(async move {
        let branch = read_branch(&root).await;
        tx.send_async(AsyncEvent::GitBranch(branch));
    });
}

async fn read_branch(root: &Path) -> Option<String> {
    let head = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    if !head.is_empty() {
        return Some(head);
    }
    // Detached: a short sha reads better in the bar than nothing at all.
    let sha = git(root, &["rev-parse", "--short", "HEAD"]).await?;
    if sha.is_empty() {
        None
    } else {
        Some(format!("@{sha}"))
    }
}

/// One `git -C <root>` invocation, trimmed; `None` when git itself fails.
async fn git(root: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        // Never prompt for credentials or a pager from inside the editor.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let output = command.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
    )
}
