//! Just enough XML to read S3 responses.
//!
//! # Why not a parser
//!
//! S3 replies with a handful of fixed, flat shapes: an upload id, a list of
//! keys and sizes, an error code and message. There are no attributes to read,
//! no namespaces to resolve, no mixed content, and no nesting that matters. A
//! full parser would be a dependency carried for four tag names.
//!
//! What this deliberately does **not** do is pretend to be general. It finds
//! text between a known open and close tag and unescapes the five XML entities.
//! Anything with attributes on the tags it is looking for, or CDATA, or a
//! namespace prefix, will not be found — and "not found" surfaces as a clear
//! error rather than a wrong value, which is the property that makes this
//! acceptable at all.

/// Text content of the first `<tag>…</tag>`, unescaped.
pub fn first(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(unescape(&xml[start..end]))
}

/// Text content of every `<tag>…</tag>`, in document order.
pub fn all(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find(&open) {
        let from = start + open.len();
        let Some(len) = rest[from..].find(&close) else {
            break;
        };
        out.push(unescape(&rest[from..from + len]));
        rest = &rest[from + len + close.len()..];
    }
    out
}

/// The five entities XML defines. No numeric references: S3 does not emit them
/// for the fields read here, and guessing at a partial implementation would be
/// worse than not finding the value.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Last, so an escaped ampersand in the source cannot be re-expanded.
        .replace("&amp;", "&")
}

/// Escape text for an XML body we send.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// An error S3 returned in a response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3ErrorBody {
    pub code: String,
    pub message: String,
}

/// Pull the `Code`/`Message` out of an error response.
///
/// Returns `None` for a body that is not an S3 error document, so a proxy's
/// HTML error page is reported as itself rather than as an empty S3 error.
pub fn parse_error(body: &str) -> Option<S3ErrorBody> {
    let code = first(body, "Code")?;
    Some(S3ErrorBody {
        message: first(body, "Message").unwrap_or_default(),
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_upload_id() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult>
  <Bucket>backups</Bucket>
  <Key>db/nightly.sql.gz</Key>
  <UploadId>2~abcdef.ghij</UploadId>
</InitiateMultipartUploadResult>"#;
        assert_eq!(first(xml, "UploadId"), Some("2~abcdef.ghij".to_string()));
        assert_eq!(first(xml, "Bucket"), Some("backups".to_string()));
    }

    #[test]
    fn a_missing_tag_is_none_not_an_empty_string() {
        // The distinction the caller needs: "the server did not send an upload
        // id" must not read as "the upload id is empty".
        assert_eq!(first("<a>1</a>", "UploadId"), None);
    }

    #[test]
    fn an_unclosed_tag_is_none() {
        assert_eq!(first("<UploadId>abc", "UploadId"), None);
    }

    #[test]
    fn reads_every_key_in_a_listing() {
        let xml = "<ListBucketResult>\
            <Contents><Key>a.sql.gz</Key><Size>10</Size></Contents>\
            <Contents><Key>b.sql.gz</Key><Size>20</Size></Contents>\
            </ListBucketResult>";
        assert_eq!(all(xml, "Key"), vec!["a.sql.gz", "b.sql.gz"]);
        assert_eq!(all(xml, "Size"), vec!["10", "20"]);
    }

    #[test]
    fn all_returns_empty_for_no_matches() {
        assert!(all("<ListBucketResult/>", "Key").is_empty());
    }

    #[test]
    fn entities_are_unescaped() {
        // Object keys carry database names, and `&` is legal in one.
        assert_eq!(
            first("<Key>a&amp;b&lt;c&gt;</Key>", "Key"),
            Some("a&b<c>".to_string())
        );
    }

    #[test]
    fn an_escaped_ampersand_does_not_re_expand() {
        // `&amp;lt;` is a literal "&lt;", not a "<". Unescaping `&amp;` first
        // would turn it into `&lt;` and then into `<`.
        assert_eq!(
            first("<Key>&amp;lt;</Key>", "Key"),
            Some("&lt;".to_string())
        );
    }

    #[test]
    fn escaping_round_trips() {
        for original in ["a&b", "a<b>c", "quote\"and'apos", "plain"] {
            let wrapped = format!("<K>{}</K>", escape(original));
            assert_eq!(first(&wrapped, "K").as_deref(), Some(original));
        }
    }

    #[test]
    fn reads_an_s3_error() {
        let xml = "<Error><Code>NoSuchBucket</Code>\
            <Message>The specified bucket does not exist</Message></Error>";
        let err = parse_error(xml).expect("an S3 error document");
        assert_eq!(err.code, "NoSuchBucket");
        assert!(err.message.contains("does not exist"));
    }

    #[test]
    fn a_non_s3_body_is_not_reported_as_an_empty_error() {
        // A proxy or load balancer in front of the endpoint returns HTML. That
        // must surface as the unexpected thing it is.
        assert_eq!(parse_error("<html><body>502</body></html>"), None);
        assert_eq!(parse_error(""), None);
    }

    #[test]
    fn a_tag_with_attributes_is_not_matched() {
        // Documents the limitation rather than hiding it: this finds `<Key>`,
        // not `<Key foo="bar">`. S3 does not use attributes on these elements.
        assert_eq!(first(r#"<Key xml:lang="en">a</Key>"#, "Key"), None);
    }
}
