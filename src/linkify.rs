//! Bare-bones URL detection for the "hold Cmd to reveal + click to open"
//! link affordance -- no `regex` dependency, just a hand-rolled scan since
//! the grammar we care about is narrow (http(s) URLs only) and this runs
//! on every visible row while Cmd is held.

const SCHEMES: [&str; 2] = ["https://", "http://"];
const TRIM_TRAILING: [char; 8] = ['.', ',', ')', ']', '}', '"', '\'', ';'];

/// Returns `(start_col, end_col_inclusive)` for every URL found in
/// `text`, one character per column exactly like the grid's own cells --
/// callers index straight into a row with these. A URL runs until
/// whitespace, then has trailing punctuation trimmed off the end rather
/// than treated as a terminator, so `(see https://example.com)` doesn't
/// swallow the `)`.
pub fn find_urls(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut urls = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let Some(scheme) = SCHEMES.iter().find(|s| rest.starts_with(*s)) else {
            i += 1;
            continue;
        };
        let mut end = i + scheme.chars().count();
        while end < chars.len() && !chars[end].is_whitespace() && !chars[end].is_control() {
            end += 1;
        }
        let mut last = end - 1;
        while last > i && TRIM_TRAILING.contains(&chars[last]) {
            last -= 1;
        }
        // A bare scheme with nothing real after it isn't a link.
        if last >= i + scheme.chars().count() {
            urls.push((i, last));
        }
        i = end.max(i + 1);
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_url() {
        assert_eq!(find_urls("visit https://example.com today"), vec![(6, 24)]);
    }

    #[test]
    fn trailing_punctuation_is_excluded() {
        let text = "(see https://example.com/path).";
        let urls = find_urls(text);
        assert_eq!(urls.len(), 1);
        let (start, end) = urls[0];
        let extracted: String = text.chars().skip(start).take(end - start + 1).collect();
        assert_eq!(extracted, "https://example.com/path");
    }

    #[test]
    fn no_url_present() {
        assert!(find_urls("just some regular text").is_empty());
    }

    #[test]
    fn multiple_urls_on_one_line() {
        assert_eq!(find_urls("http://a.com and https://b.com").len(), 2);
    }

    #[test]
    fn bare_scheme_is_not_a_link() {
        assert!(find_urls("http://").is_empty());
        assert!(find_urls("nothing after http:// here").is_empty());
    }

    #[test]
    fn url_at_end_of_line() {
        assert_eq!(find_urls("go to http://x.io"), vec![(6, 16)]);
    }
}
