//! `text/uri-list` (RFC 2483): one URI per line, CRLF-separated, `#` lines are comments. File
//! managers put `file://` URIs in it, percent-encoded (RFC 8089); anything else (`http:`, a
//! remote host) isn't a local file and is skipped.

use std::path::PathBuf;

/// The MIME type a drag of files carries.
pub const MIME: &str = "text/uri-list";

/// The local file paths in a `text/uri-list` payload, in order.
pub fn file_paths(data: &[u8]) -> Vec<PathBuf> {
    data.split(|&b| b == b'\n').filter_map(|line| file_path(line.strip_suffix(b"\r").unwrap_or(line))).collect()
}

/// The path of one `file:` URI line (`None` for comments, blanks, other schemes and remote
/// hosts).
fn file_path(line: &[u8]) -> Option<PathBuf> {
    let line = line.trim_ascii();
    if !line.get(..5).is_some_and(|s| s.eq_ignore_ascii_case(b"file:")) {
        return None;
    }
    let rest = line.get(5..)?;
    // `file:///p` and `file://localhost/p` are local; `file://host/p` names another machine.
    // `file:/p` (no authority) is local too.
    let path = match rest.strip_prefix(b"//") {
        Some(authority_path) => {
            let slash = authority_path.iter().position(|&b| b == b'/')?;
            let host = authority_path.get(..slash)?;
            if !host.is_empty() && !host.eq_ignore_ascii_case(b"localhost") {
                return None;
            }
            authority_path.get(slash..)?
        }
        None => rest,
    };
    if path.first() != Some(&b'/') {
        return None;
    }
    // A query or fragment isn't part of the path.
    let path = path.split(|&b| b == b'?' || b == b'#').next()?;
    let bytes = percent_decode(path);
    // A NUL can't be in a path.
    if bytes.contains(&0) {
        return None;
    }
    Some(to_path(bytes))
}

/// Decode `%XX` escapes; a `%` not followed by two hex digits stays as it is.
fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(&b) = s.get(i) {
        if b == b'%'
            && let (Some(hi), Some(lo)) = (s.get(i + 1).copied().and_then(hex), s.get(i + 2).copied().and_then(hex))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}

/// Paths are bytes on Unix (not necessarily UTF-8).
#[cfg(unix)]
fn to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(data: &str) -> Vec<String> {
        file_paths(data.as_bytes()).iter().map(|p| p.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn file_uris_become_paths_in_order() {
        let list = "file:///home/a/one.png\r\nfile:///home/a/two.psd\r\n";
        assert_eq!(paths(list), ["/home/a/one.png", "/home/a/two.psd"]);
        // Without the trailing CRLF, with bare LF, and with surrounding blanks.
        assert_eq!(paths("file:///x.png"), ["/x.png"]);
        assert_eq!(paths("  file:///x.png  \n\nfile:///y.png"), ["/x.png", "/y.png"]);
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(paths("file:///home/a/My%20Photo%2B1.jpg"), ["/home/a/My Photo+1.jpg"]);
        assert_eq!(paths("file:///f%C3%B6t%C3%B3.png"), ["/fötó.png"]);
        // Malformed escapes stay literal.
        assert_eq!(paths("file:///a%2.png\nfile:///b%zz.png\nfile:///c%"), ["/a%2.png", "/b%zz.png", "/c%"]);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_survive() {
        use std::os::unix::ffi::OsStrExt;
        let p = file_paths(b"file:///a%FF.png");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].as_os_str().as_bytes(), b"/a\xFF.png");
    }

    #[test]
    fn local_hosts_and_short_forms_are_accepted() {
        assert_eq!(paths("file://localhost/x.png"), ["/x.png"]);
        assert_eq!(paths("FILE://LOCALHOST/x.png"), ["/x.png"]);
        assert_eq!(paths("file:/x.png"), ["/x.png"]);
    }

    #[test]
    fn everything_else_is_skipped() {
        let list = "# a comment\r\nhttps://example.com/x.png\r\nfile://otherhost/x.png\r\nfile:relative.png\r\nfile:\r\nfile://\r\nfile:///nul%00.png\r\nfile:///ok.png";
        assert_eq!(paths(list), ["/ok.png"]);
        assert!(paths("").is_empty());
        assert!(file_paths(&[0xFF, 0xFE, b'\n', b'%']).is_empty());
    }

    #[test]
    fn query_and_fragment_are_dropped() {
        assert_eq!(paths("file:///x.png?size=2#top"), ["/x.png"]);
        // Encoded, they are part of the name.
        assert_eq!(paths("file:///x%3F%23.png"), ["/x?#.png"]);
    }
}
