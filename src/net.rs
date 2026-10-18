//! Small networking helpers.
//!
//! The session cookie is an account-owning credential. Everything that carries
//! it must decide, per-URL, whether the destination is Slack's; a URL that
//! arrived inside message metadata (a file's `url_private`, an avatar or a
//! custom emoji) is not trusted to be Slack-hosted just because it is shaped
//! like it.

/// True when `url`'s host belongs to Slack.
///
/// Used to gate the `d` session cookie: image and file downloads must send it
/// to Slack's own hosts only, or a crafted URL leaks the session to a third
/// party. Anything that parses as non-Slack (or does not parse at all) returns
/// `false`, so the safe default is "do not send the cookie".
pub fn is_slack_host(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    // Lowercase and strip a trailing dot so `slack.com.` cannot slip past.
    let Some(host) = parsed
        .host_str()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
    else {
        return false;
    };

    const SLACK_DOMAINS: &[&str] = &[
        "slack.com",
        "slack-edge.com",
        "slack-msgs.com",
        "slack-imgs.com",
    ];

    SLACK_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_hosts_are_recognised() {
        for u in [
            "https://files.slack.com/files-pri/T1-F1/x.png",
            "https://a.slack-edge.com/xxx/emoji.png",
            "https://avatars.slack-edge.com/xxx.webp",
            "https://slack.com/x",
            "https://ca.slack-edge.com/x",
        ] {
            assert!(is_slack_host(u), "{u}");
        }
    }

    #[test]
    fn third_parties_and_lookalikes_are_rejected() {
        for u in [
            "https://evil.com/x.png",
            "https://slack.com.evil.com/x.png",
            "https://notslack.com/x.png",
            "https://evil-slack-edge.com/x.png",
            "https://slack-edge.com.evil.com/x",
            "http://localhost/x.png",
            "not a url",
            "//evil.com/x.png",
        ] {
            assert!(!is_slack_host(u), "{u}");
        }
    }

    #[test]
    fn a_trailing_dot_cannot_bypass_the_check() {
        assert!(is_slack_host("https://files.slack.com./x.png"));
    }
}
