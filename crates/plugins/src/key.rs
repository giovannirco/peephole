/// Make user data (camera names, label values) safe as one store key segment.
pub(crate) fn sanitize(raw: &str) -> String {
    raw.chars().map(|c| if c == '/' || c.is_whitespace() { '-' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slashes_and_whitespace_become_dashes() {
        assert_eq!(sanitize("Front door"), "Front-door");
        assert_eq!(sanitize("a/b\tc\nd"), "a-b-c-d");
        assert_eq!(sanitize("  pad "), "--pad-");
        assert_eq!(sanitize("plain"), "plain");
    }
}
