//! Prompt and identifier input for a delegation: an optional positional prompt, or a prompt read
//! from a file / stdin, and the validated `--task-id` / `--review-id` labels.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

/// Largest prompt accepted from a file or stdin. A review brief is text, not a data dump.
pub const MAX_PROMPT_BYTES: u64 = 1024 * 1024;
/// Longest accepted task / review identifier.
pub const MAX_ID_CHARS: usize = 128;

/// clap value parser: a label made of `[A-Za-z0-9._:/-]`, 1..=128 chars. It lands in a receipt and
/// a log line, so nothing that could carry whitespace, control characters or a secret-shaped blob
/// of free text is accepted.
pub fn parse_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err("identifier cannot be empty".into());
    }
    if id.chars().count() > MAX_ID_CHARS {
        return Err(format!("identifier longer than {MAX_ID_CHARS} characters"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
    {
        return Err("identifier may only use letters, digits and . _ : / -".into());
    }
    Ok(id.to_owned())
}

/// Read at most [`MAX_PROMPT_BYTES`] of UTF-8 text; larger input is refused, never truncated.
fn read_bounded(mut r: impl Read, what: &str) -> Result<String> {
    let mut buf = Vec::new();
    r.by_ref()
        .take(MAX_PROMPT_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("cannot read {what}"))?;
    if buf.len() as u64 > MAX_PROMPT_BYTES {
        bail!("{what} is larger than {MAX_PROMPT_BYTES} bytes");
    }
    String::from_utf8(buf).map_err(|_| anyhow::anyhow!("{what} is not valid UTF-8"))
}

/// The prompt text: the positional argument, or the contents of `file` (`-` is stdin). Exactly one
/// must be given (clap enforces this; it is re-checked here so the function is safe on its own).
pub fn resolve_prompt(
    positional: Option<String>,
    file: Option<&Path>,
    stdin: impl Read,
) -> Result<String> {
    let prompt = match (positional, file) {
        (Some(_), Some(_)) => bail!("give a prompt or --prompt-file, not both"),
        (None, None) => bail!("a prompt or --prompt-file is required"),
        (Some(p), None) => p,
        (None, Some(f)) if f == Path::new("-") => read_bounded(stdin, "stdin prompt")?,
        (None, Some(f)) => {
            let fh = std::fs::File::open(f)
                .with_context(|| format!("cannot open prompt file {}", f.display()))?;
            read_bounded(fh, &format!("prompt file {}", f.display()))?
        }
    };
    if prompt.trim().is_empty() {
        bail!("prompt cannot be empty");
    }
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cross-agent-input-{name}-{}", std::process::id()))
    }

    #[test]
    fn ids_are_labels_only() {
        assert_eq!(parse_id(" task-1.a:b/c_d ").unwrap(), "task-1.a:b/c_d");
        for bad in [
            "",
            "   ",
            "has space",
            "semi;colon",
            "new\nline",
            "ünï",
            "$(x)",
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?}");
        }
        assert!(parse_id(&"a".repeat(MAX_ID_CHARS)).is_ok());
        assert!(parse_id(&"a".repeat(MAX_ID_CHARS + 1)).is_err());
    }

    #[test]
    fn positional_prompt_passes_through_and_stdin_is_untouched() {
        let p = resolve_prompt(Some("hello".into()), None, std::io::empty()).unwrap();
        assert_eq!(p, "hello");
    }

    #[test]
    fn prompt_sources_are_exclusive_and_required() {
        let f = Path::new("/nonexistent");
        assert!(resolve_prompt(Some("x".into()), Some(f), std::io::empty()).is_err());
        assert!(resolve_prompt(None, None, std::io::empty()).is_err());
        assert!(resolve_prompt(Some("  \n".into()), None, std::io::empty()).is_err());
    }

    #[test]
    fn dash_reads_stdin_and_blank_or_binary_is_refused() {
        let dash = Some(Path::new("-"));
        let got = resolve_prompt(None, dash, "from stdin\n".as_bytes()).unwrap();
        assert_eq!(got, "from stdin\n");
        assert!(resolve_prompt(None, dash, "  \n".as_bytes()).is_err());
        assert!(resolve_prompt(None, dash, &[0xff_u8, 0xfe][..]).is_err());
    }

    #[test]
    fn file_prompt_is_read_and_bad_files_are_refused() {
        let ok = scratch("ok");
        std::fs::write(&ok, "from file").unwrap();
        assert_eq!(
            resolve_prompt(None, Some(&ok), std::io::empty()).unwrap(),
            "from file"
        );
        let _ = std::fs::remove_file(&ok);
        assert!(resolve_prompt(None, Some(&ok), std::io::empty()).is_err());
        // A directory is not a prompt.
        assert!(resolve_prompt(None, Some(&std::env::temp_dir()), std::io::empty()).is_err());
    }

    #[test]
    fn oversized_input_is_refused_not_truncated() {
        let big = vec![b'a'; (MAX_PROMPT_BYTES + 1) as usize];
        assert!(resolve_prompt(None, Some(Path::new("-")), &big[..]).is_err());
        let exact = vec![b'a'; MAX_PROMPT_BYTES as usize];
        assert!(resolve_prompt(None, Some(Path::new("-")), &exact[..]).is_ok());
    }
}
