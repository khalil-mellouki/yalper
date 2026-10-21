//! The viewer's files, embedded in the binary.
//!
//! The viewer's source is in `web/` at the repository root. `npm run build` writes the built files into
//! `crates/yalper/viewer/`, where they are committed (CI checks that they match the source), so building Yalper
//! never runs Node. They are embedded at compile time in every build, debug and tests included, and nothing is
//! read from disk at run time.

use std::borrow::Cow;

use rust_embed::Embed;

#[derive(Embed)]
#[folder = "viewer/"]
struct Viewer;

/// One embedded file of the viewer.
#[derive(Debug)]
pub struct Asset {
    pub bytes: Cow<'static, [u8]>,
    /// The value of the `Content-Type` header to serve it with.
    pub content_type: &'static str,
}

/// The embedded file at `path`, relative to the viewer folder with `/` separators (`index.html`,
/// `assets/index.js`). `None` if there is no such file, or if its extension is not one the viewer uses.
pub fn asset(path: &str) -> Option<Asset> {
    // rust-embed would turn `\` into `/`: refused, so that each file has one name only.
    if path.contains('\\') {
        return None;
    }
    let content_type = content_type(path)?;
    let file = Viewer::get(path)?;
    Some(Asset {
        bytes: file.data,
        content_type,
    })
}

/// The content type of each extension Vite writes into the viewer. Other files are not served.
fn content_type(path: &str) -> Option<&'static str> {
    let (_, extension) = path.rsplit_once('.')?;
    Some(match extension {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = "text/html; charset=utf-8";
    const JAVASCRIPT: &str = "text/javascript; charset=utf-8";
    const CSS: &str = "text/css; charset=utf-8";

    /// The value of the attribute `name` in the start tag `tag` (the text after `<`).
    fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let start = tag.find(&format!(" {name}=\""))? + name.len() + 3;
        let length = tag[start..].find('"')?;
        Some(&tag[start..start + length])
    }

    #[test]
    fn index_references_only_embedded_files_of_the_right_type() {
        let index = asset("index.html").expect("index.html is embedded");
        assert_eq!(index.content_type, HTML);
        let html = std::str::from_utf8(&index.bytes).expect("index.html is UTF-8");

        let mut scripts = 0;
        let mut stylesheets = 0;
        for tag in html.split('<').skip(1) {
            let tag = &tag[..tag.find('>').expect("tags are closed")];
            let name = tag.split_whitespace().next().unwrap_or_default();
            let (reference, expected) = match name {
                "script" => {
                    scripts += 1;
                    let src = attribute(tag, "src");
                    assert!(src.is_some(), "inline script in index.html: <{tag}>");
                    (src, JAVASCRIPT)
                }
                "link" => match attribute(tag, "rel") {
                    Some("stylesheet") => {
                        stylesheets += 1;
                        (attribute(tag, "href"), CSS)
                    }
                    Some("modulepreload") => (attribute(tag, "href"), JAVASCRIPT),
                    rel => panic!("unexpected link {rel:?} in index.html: <{tag}>"),
                },
                _ => {
                    assert!(
                        attribute(tag, "src").is_none() && attribute(tag, "href").is_none(),
                        "unexpected reference in index.html: <{tag}>"
                    );
                    continue;
                }
            };
            let reference = reference.unwrap_or_else(|| panic!("no reference in <{tag}>"));
            let path = reference
                .strip_prefix('/')
                .filter(|path| !path.starts_with('/'))
                .unwrap_or_else(|| panic!("{reference} is not a path on the viewer's own server"));
            let file = asset(path).unwrap_or_else(|| panic!("{path} is not embedded"));
            assert_eq!(file.content_type, expected, "content type of {path}");
        }
        assert!(scripts > 0, "index.html loads no script");
        assert!(stylesheets > 0, "index.html loads no stylesheet");
    }

    #[test]
    fn every_embedded_file_is_served() {
        let mut files = 0;
        for path in Viewer::iter() {
            assert!(asset(&path).is_some(), "{path} is embedded but not served");
            files += 1;
        }
        assert!(files >= 3, "only {files} files embedded");
        assert_eq!(
            asset("licenses.json").map(|file| file.content_type),
            Some("application/json")
        );
    }

    #[test]
    fn only_embedded_files_with_a_known_extension_are_served() {
        for path in [
            "missing.js",
            "assets",
            "/index.html",
            "./index.html",
            "../Cargo.toml",
            "../src/lib.rs",
            "assets\\index.js",
            "",
        ] {
            assert!(asset(path).is_none(), "{path:?} served");
        }
        for path in [
            "index.js.map",
            "app.wasm",
            "favicon.ico",
            "index",
            "a.html/b",
        ] {
            assert_eq!(content_type(path), None, "{path}");
        }
        assert_eq!(content_type("assets/index.js"), Some(JAVASCRIPT));
        assert_eq!(content_type("assets/index.css"), Some(CSS));
    }
}
