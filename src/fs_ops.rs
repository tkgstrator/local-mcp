use std::{fmt::Write as _, fs::Metadata, io::ErrorKind, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use ignore::WalkBuilder;
use regex::Regex;

use crate::root::Root;

/// A NUL byte in the first chunk is the usual signal that a file is not text.
const BINARY_SNIFF: usize = 8192;

/// A resolved path written the way the caller would: relative to the root, and
/// `.` for the root itself, which relativizes to nothing at all.
fn shown(root: &Root, path: &Path) -> String {
    let relative = root.relativize(path).display().to_string();
    if relative.is_empty() {
        ".".to_owned()
    } else {
        relative
    }
}

/// An io failure said in terms of what the caller can do about it.
///
/// `No such file or directory (os error 2)` describes the syscall's problem
/// rather than theirs: what they are missing is that paths are read against the
/// sandbox root, and which tool would have shown them what is really there.
fn cannot(verb: &str, shown: &str, error: std::io::Error) -> anyhow::Error {
    match error.kind() {
        ErrorKind::NotFound => anyhow!(
            "no such file: {shown} (paths are relative to the sandbox root; list_dir shows \
             what is there)"
        ),
        ErrorKind::IsADirectory => {
            anyhow!("{shown} is a directory, not a file; list_dir lists it")
        },
        ErrorKind::PermissionDenied => anyhow!("not allowed to {verb} {shown}"),
        _ => anyhow::Error::new(error).context(format!("cannot {verb} {shown}")),
    }
}

/// Both walkers below simply yield nothing for a path that is not there, so the
/// answer comes back as an empty listing or as no matches — which reads as
/// "nothing here" when what happened is "nowhere like that".
async fn present(root: &Root, resolved: &Path) -> Result<Metadata> {
    tokio::fs::metadata(resolved).await.map_err(|error| {
        let shown = shown(root, resolved);
        match error.kind() {
            ErrorKind::NotFound => anyhow!(
                "no such path: {shown} (paths are relative to the sandbox root; list_dir shows \
                 what is there)"
            ),
            _ => anyhow::Error::new(error).context(format!("cannot open {shown}")),
        }
    })
}

fn truncate(mut text: String, max_output: usize) -> String {
    if text.len() > max_output {
        text.truncate(max_output);
        text.push_str("\n... [truncated]");
    }
    text
}

pub async fn read_file(
    root: &Root,
    path: &str,
    offset: Option<u32>,
    limit: Option<u32>,
    max_output: usize,
) -> Result<String> {
    let resolved = root.resolve(path)?;
    let shown = shown(root, &resolved);
    let bytes = tokio::fs::read(&resolved)
        .await
        .map_err(|error| cannot("read", &shown, error))?;

    if bytes.iter().take(BINARY_SNIFF).any(|b| *b == 0) {
        bail!("{shown} looks like a binary file ({} bytes)", bytes.len());
    }

    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = offset.unwrap_or(0) as usize;
    let end = limit
        .map(|l| start.saturating_add(l as usize).min(lines.len()))
        .unwrap_or(lines.len());

    if start >= lines.len() && !lines.is_empty() {
        bail!(
            "offset {start} is past the end of {shown} ({} lines)",
            lines.len()
        );
    }

    let mut out = String::new();
    for (index, line) in lines[start.min(lines.len())..end].iter().enumerate() {
        let _ = writeln!(out, "{:>6}\t{line}", start + index + 1);
    }
    Ok(truncate(out, max_output))
}

pub async fn write_file(root: &Root, path: &str, content: &str) -> Result<String> {
    let resolved = root.resolve(path)?;
    let shown = shown(root, &resolved);
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create a parent directory for {shown}"))?;
    }
    tokio::fs::write(&resolved, content)
        .await
        .map_err(|error| cannot("write", &shown, error))?;
    Ok(format!("wrote {} bytes to {shown}", content.len()))
}

pub async fn edit_file(root: &Root, path: &str, old_text: &str, new_text: &str) -> Result<String> {
    if old_text.is_empty() {
        bail!("old_text must not be empty; use write_file to create a file");
    }
    let resolved = root.resolve(path)?;
    let shown = shown(root, &resolved);
    let current = tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|error| cannot("read", &shown, error))?;

    // Refuse ambiguous edits rather than guessing which occurrence was meant.
    match current.matches(old_text).count() {
        0 => bail!("old_text does not appear in {shown}"),
        1 => {},
        n => bail!("old_text appears {n} times in {shown}; make it unique"),
    }

    let updated = current.replace(old_text, new_text);
    tokio::fs::write(&resolved, &updated)
        .await
        .map_err(|error| cannot("write", &shown, error))?;
    Ok(format!("edited {shown}"))
}

pub async fn list_dir(
    root: &Root,
    path: Option<&str>,
    depth: Option<u32>,
    max_output: usize,
) -> Result<String> {
    let resolved = root.resolve(path.unwrap_or("."))?;
    if !present(root, &resolved).await?.is_dir() {
        bail!(
            "{} is a file, not a directory; read_file reads it",
            shown(root, &resolved)
        );
    }

    let walker = WalkBuilder::new(&resolved)
        .max_depth(Some(depth.unwrap_or(1) as usize))
        .hidden(false)
        .git_ignore(true)
        .build();

    let mut out = String::new();
    for entry in walker.flatten() {
        if entry.path() == resolved {
            continue;
        }
        let relative = entry.path().strip_prefix(&resolved).unwrap_or(entry.path());
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if is_dir {
            let _ = writeln!(out, "{}/", relative.display());
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = writeln!(out, "{}\t{size}", relative.display());
        }
    }

    if out.is_empty() {
        out.push_str("(empty)");
    }
    Ok(truncate(out, max_output))
}

pub async fn search(
    root: &Root,
    pattern: &str,
    path: Option<&str>,
    max_results: Option<u32>,
    max_output: usize,
) -> Result<String> {
    let regex = Regex::new(pattern).context("pattern is not a valid regular expression")?;
    let resolved = root.resolve(path.unwrap_or("."))?;
    present(root, &resolved).await?;
    let limit = max_results.unwrap_or(200) as usize;

    let mut out = String::new();
    let mut hits = 0usize;
    for entry in WalkBuilder::new(&resolved).hidden(false).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.iter().take(BINARY_SNIFF).any(|b| *b == 0) {
            continue;
        }
        let shown = shown(root, entry.path());
        for (number, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if regex.is_match(line) {
                let _ = writeln!(out, "{shown}:{}:{}", number + 1, line.trim_end());
                hits += 1;
                if hits >= limit {
                    let _ = writeln!(out, "... [stopped at {limit} matches]");
                    return Ok(truncate(out, max_output));
                }
            }
        }
    }

    if out.is_empty() {
        out.push_str("(no matches)");
    }
    Ok(truncate(out, max_output))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 65536;

    fn sandbox() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), b"hi\n").unwrap();
        let root = Root::new(dir.path()).unwrap();
        (dir, root)
    }

    fn message(error: anyhow::Error) -> String {
        format!("{error:#}")
    }

    /// `No such file or directory (os error 2)` is true and useless: it says
    /// nothing about the root the path was read against, and nothing about the
    /// tool that would have shown what is really there.
    #[tokio::test]
    async fn a_missing_file_says_where_paths_are_read_from() {
        let (_dir, root) = sandbox();
        let error = message(
            read_file(&root, "sub/nope.txt", None, None, MAX)
                .await
                .unwrap_err(),
        );

        assert!(error.contains("no such file: sub/nope.txt"), "{error}");
        assert!(error.contains("sandbox root"), "{error}");
        assert!(error.contains("list_dir"), "{error}");
        assert!(!error.contains("os error"), "{error}");
    }

    /// Reaching for a directory with the wrong tool is a mistake worth naming,
    /// rather than passing `Is a directory (os error 21)` along.
    #[tokio::test]
    async fn reading_a_directory_names_the_tool_that_lists_it() {
        let (_dir, root) = sandbox();
        let error = message(read_file(&root, "sub", None, None, MAX).await.unwrap_err());

        assert!(error.contains("sub is a directory"), "{error}");
        assert!(error.contains("list_dir"), "{error}");
    }

    /// The walker yields nothing for a path that is not there, so without this
    /// the answer is `(empty)` — which says the directory is empty, not that it
    /// does not exist.
    #[tokio::test]
    async fn listing_somewhere_that_is_not_there_says_so() {
        let (_dir, root) = sandbox();
        let error = message(list_dir(&root, Some("nope"), None, MAX).await.unwrap_err());

        assert!(error.contains("no such path: nope"), "{error}");
    }

    #[tokio::test]
    async fn listing_a_file_names_the_tool_that_reads_it() {
        let (_dir, root) = sandbox();
        let error = message(
            list_dir(&root, Some("sub/file.txt"), None, MAX)
                .await
                .unwrap_err(),
        );

        assert!(error.contains("is a file, not a directory"), "{error}");
        assert!(error.contains("read_file"), "{error}");
    }

    /// Same trap as the listing above, with `(no matches)` as the answer that
    /// looks like a successful search of the wrong place.
    #[tokio::test]
    async fn searching_somewhere_that_is_not_there_says_so() {
        let (_dir, root) = sandbox();
        let error = message(
            search(&root, "hi", Some("nope"), None, MAX)
                .await
                .unwrap_err(),
        );

        assert!(error.contains("no such path: nope"), "{error}");
    }

    /// The root relativizes to an empty string, which would leave the message
    /// with a hole in it where the path should be.
    #[tokio::test]
    async fn the_root_itself_is_shown_as_a_path() {
        let (_dir, root) = sandbox();
        let error = message(read_file(&root, ".", None, None, MAX).await.unwrap_err());

        assert!(error.starts_with(". is a directory"), "{error}");
    }
}
