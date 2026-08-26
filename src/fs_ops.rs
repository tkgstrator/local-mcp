use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use regex::Regex;

use crate::root::Root;

/// A NUL byte in the first chunk is the usual signal that a file is not text.
const BINARY_SNIFF: usize = 8192;

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
    let shown = root.relativize(&resolved).display().to_string();
    let bytes = tokio::fs::read(&resolved)
        .await
        .with_context(|| format!("cannot read {shown}"))?;

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
        bail!("offset {start} is past the end of {shown} ({} lines)", lines.len());
    }

    let mut out = String::new();
    for (index, line) in lines[start.min(lines.len())..end].iter().enumerate() {
        let _ = writeln!(out, "{:>6}\t{line}", start + index + 1);
    }
    Ok(truncate(out, max_output))
}

pub async fn write_file(root: &Root, path: &str, content: &str) -> Result<String> {
    let resolved = root.resolve(path)?;
    let shown = root.relativize(&resolved).display().to_string();
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("cannot create parent directory for {shown}"))?;
    }
    tokio::fs::write(&resolved, content)
        .await
        .with_context(|| format!("cannot write {shown}"))?;
    Ok(format!("wrote {} bytes to {shown}", content.len()))
}

pub async fn edit_file(
    root: &Root,
    path: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String> {
    if old_text.is_empty() {
        bail!("old_text must not be empty; use write_file to create a file");
    }
    let resolved = root.resolve(path)?;
    let shown = root.relativize(&resolved).display().to_string();
    let current = tokio::fs::read_to_string(&resolved)
        .await
        .with_context(|| format!("cannot read {shown}"))?;

    // Refuse ambiguous edits rather than guessing which occurrence was meant.
    match current.matches(old_text).count() {
        0 => bail!("old_text does not appear in {shown}"),
        1 => {}
        n => bail!("old_text appears {n} times in {shown}; make it unique"),
    }

    let updated = current.replace(old_text, new_text);
    tokio::fs::write(&resolved, &updated)
        .await
        .with_context(|| format!("cannot write {shown}"))?;
    Ok(format!("edited {shown}"))
}

pub async fn list_dir(
    root: &Root,
    path: Option<&str>,
    depth: Option<u32>,
    max_output: usize,
) -> Result<String> {
    let resolved = root.resolve(path.unwrap_or("."))?;
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
        let shown = root.relativize(entry.path()).display().to_string();
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
