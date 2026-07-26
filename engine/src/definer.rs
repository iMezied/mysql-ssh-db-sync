//! Streaming `DEFINER=` removal for MySQL dumps.
//!
//! Restoring a dump that names a definer requires SUPER (or SET_USER_ID) on the
//! destination, which a migration user rarely has. `mysqldump` has no flag to
//! omit it — `--skip-definer` is a MySQL *Shell* feature and does not exist on
//! `mysqldump`, so it must be filtered out of the stream.
//!
//! The bash predecessor ran `sed 's/DEFINER=[^ ]* / /g'` over the finished file.
//! That is wrong twice over: it rewrites occurrences inside string literals and
//! row data, and it needs a second full pass over a multi-gigabyte file. This
//! filter is quote-aware and runs inline on the stream.

use std::borrow::Cow;
use std::io::{BufRead, Write};

/// Remove `DEFINER=<user>@<host>` clauses that appear outside string literals.
///
/// Returns `Cow::Borrowed` when nothing changed, so unaffected lines (the vast
/// majority) cost no allocation.
pub fn strip_definers(line: &str) -> Cow<'_, str> {
    // Cheap reject: most lines have no DEFINER at all.
    if !contains_ignore_ascii_case(line, "DEFINER=") {
        return Cow::Borrowed(line);
    }

    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut changed = false;

    while i < bytes.len() {
        let c = bytes[i];

        if let Some(q) = quote {
            out.push(c as char);
            if c == b'\\' && q != b'`' {
                // Backslash escapes apply inside ' and " but not inside
                // backticks. Copy the escaped byte verbatim.
                if i + 1 < bytes.len() {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
            } else if c == q {
                // A doubled quote is an escaped quote, not a terminator.
                if i + 1 < bytes.len() && bytes[i + 1] == q {
                    out.push(q as char);
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }

        if c == b'\'' || c == b'"' || c == b'`' {
            quote = Some(c);
            out.push(c as char);
            i += 1;
            continue;
        }

        if matches_ignore_ascii_case(&bytes[i..], b"DEFINER=") {
            let after = skip_definer_clause(bytes, i + b"DEFINER=".len());
            // Collapse the trailing space so `CREATE DEFINER=x VIEW` becomes
            // `CREATE VIEW`, not `CREATE  VIEW`.
            let after = if after < bytes.len() && bytes[after] == b' ' {
                after + 1
            } else {
                after
            };
            i = after;
            changed = true;
            continue;
        }

        out.push(c as char);
        i += 1;
    }

    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(line)
    }
}

/// Advance past `<user>@<host>` (or a bare token such as `CURRENT_USER`).
fn skip_definer_clause(bytes: &[u8], mut i: usize) -> usize {
    i = skip_definer_part(bytes, i);
    if i < bytes.len() && bytes[i] == b'@' {
        i = skip_definer_part(bytes, i + 1);
    }
    i
}

/// Advance past one quoted or bare identifier.
fn skip_definer_part(bytes: &[u8], mut i: usize) -> usize {
    if i >= bytes.len() {
        return i;
    }

    let c = bytes[i];
    if c == b'`' || c == b'\'' || c == b'"' {
        i += 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' && c != b'`' {
                i += 2;
                continue;
            }
            if bytes[i] == c {
                // Doubled quote inside the identifier.
                if i + 1 < bytes.len() && bytes[i + 1] == c {
                    i += 2;
                    continue;
                }
                return i + 1;
            }
            i += 1;
        }
        return i;
    }

    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$')) {
        i += 1;
    }
    i
}

fn matches_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Apply [`strip_definers`] to every line of a stream.
///
/// Returns the number of lines that were modified, for the job log.
pub fn strip_definers_stream<R: BufRead, W: Write>(
    reader: R,
    writer: &mut W,
) -> std::io::Result<u64> {
    let mut modified = 0u64;
    for line in reader.lines() {
        let line = line?;
        match strip_definers(&line) {
            Cow::Borrowed(s) => writeln!(writer, "{s}")?,
            Cow::Owned(s) => {
                modified += 1;
                writeln!(writer, "{s}")?;
            }
        }
    }
    Ok(modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_backtick_quoted_definer() {
        let input = "/*!50013 DEFINER=`root`@`localhost` SQL SECURITY DEFINER */";
        let out = strip_definers(input);
        assert!(!out.contains("DEFINER=`root`"));
        assert_eq!(out, "/*!50013 SQL SECURITY DEFINER */");
    }

    #[test]
    fn strips_single_quoted_definer() {
        let input = "CREATE DEFINER='admin'@'%' PROCEDURE `p`()";
        assert_eq!(strip_definers(input), "CREATE PROCEDURE `p`()");
    }

    #[test]
    fn strips_bare_current_user() {
        let input = "CREATE DEFINER=CURRENT_USER VIEW `v` AS SELECT 1";
        assert_eq!(strip_definers(input), "CREATE VIEW `v` AS SELECT 1");
    }

    #[test]
    fn strips_multiple_occurrences_on_one_line() {
        let input = "DEFINER=`a`@`b` X DEFINER=`c`@`d` Y";
        assert_eq!(strip_definers(input), "X Y");
    }

    #[test]
    fn leaves_definer_inside_single_quoted_string_alone() {
        // This is row data, not a clause. The old sed corrupted it.
        let input = "INSERT INTO `logs` VALUES ('DEFINER=`root`@`localhost` was set');";
        assert_eq!(
            strip_definers(input),
            input,
            "DEFINER inside a string literal is data and must survive"
        );
    }

    #[test]
    fn leaves_definer_inside_double_quoted_string_alone() {
        let input = r#"INSERT INTO `t` VALUES ("DEFINER=`x`@`y`");"#;
        assert_eq!(strip_definers(input), input);
    }

    #[test]
    fn handles_escaped_quotes_in_data() {
        let input = r"INSERT INTO `t` VALUES ('it\'s DEFINER=`a`@`b` here');";
        assert_eq!(
            strip_definers(input),
            input,
            "an escaped quote must not end the string early"
        );
    }

    #[test]
    fn handles_doubled_quotes_in_data() {
        let input = "INSERT INTO `t` VALUES ('it''s DEFINER=`a`@`b` here');";
        assert_eq!(strip_definers(input), input);
    }

    #[test]
    fn strips_clause_but_keeps_later_string_data() {
        let input = "CREATE DEFINER=`root`@`localhost` TRIGGER t BEFORE INSERT ON `x` \
                     FOR EACH ROW SET @m = 'DEFINER=`root`@`localhost`';";
        let out = strip_definers(input);
        assert!(out.starts_with("CREATE TRIGGER"));
        assert!(
            out.contains("'DEFINER=`root`@`localhost`'"),
            "the literal inside the trigger body must be preserved"
        );
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(
            strip_definers("CREATE definer=`a`@`b` VIEW v"),
            "CREATE VIEW v"
        );
    }

    #[test]
    fn unaffected_lines_are_not_reallocated() {
        let input = "INSERT INTO `t` VALUES (1,2,3);";
        assert!(matches!(strip_definers(input), Cow::Borrowed(_)));
    }

    #[test]
    fn definer_with_at_sign_in_quoted_host() {
        let input = "CREATE DEFINER=`user`@`host@domain` VIEW v";
        assert_eq!(strip_definers(input), "CREATE VIEW v");
    }

    #[test]
    fn stream_reports_modified_line_count() {
        let input = "SELECT 1;\nCREATE DEFINER=`a`@`b` VIEW v AS SELECT 1;\nSELECT 2;\n";
        let mut out = Vec::new();
        let modified = strip_definers_stream(input.as_bytes(), &mut out).unwrap();

        assert_eq!(modified, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("DEFINER"));
        assert!(text.contains("SELECT 1;"));
        assert!(text.contains("SELECT 2;"));
    }

    #[test]
    fn stream_preserves_unrelated_content_exactly() {
        let input = "line one\nline two\n";
        let mut out = Vec::new();
        strip_definers_stream(input.as_bytes(), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), input);
    }

    #[test]
    fn truncated_definer_at_end_of_line_does_not_panic() {
        assert_eq!(strip_definers("CREATE DEFINER="), "CREATE ");
        assert_eq!(strip_definers("CREATE DEFINER=`unterminated"), "CREATE ");
    }
}
