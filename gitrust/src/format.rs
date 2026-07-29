use chrono::{Duration, Utc};

/// Format a unix epoch timestamp as a human-friendly relative string.
/// Clock skew producing negative deltas is clamped to "just now".
pub fn format_relative(epoch_secs: i64) -> String {
    let now = Utc::now().timestamp();
    let delta = now - epoch_secs;
    if delta < 60 {
        return "just now".to_string();
    }
    let d = Duration::seconds(delta);
    if d.num_minutes() < 60 {
        return format!("{}m ago", d.num_minutes());
    }
    if d.num_hours() < 24 {
        return format!("{}h ago", d.num_hours());
    }
    if d.num_days() < 30 {
        return format!("{}d ago", d.num_days());
    }
    if d.num_days() < 365 {
        return format!("{}mo ago", d.num_days() / 30);
    }
    format!("{}y ago", d.num_days() / 365)
}

/// Translate a git remote URL (SSH or HTTPS) into a browser URL.
/// Returns None for unknown hosts — never guesses.
///
/// Supported hosts: github.com, gitlab.com, bitbucket.org.
pub fn remote_to_web_url(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = parse_host_and_path(url)?;
    if !is_known_host(&host) {
        return None;
    }
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    if path.is_empty() {
        return None;
    }
    Some(format!("https://{}/{}", host, path))
}

fn parse_host_and_path(url: &str) -> Option<(String, String)> {
    // SSH form: git@host:owner/repo(.git)
    if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some((host.to_string(), path.to_string()));
    }
    // ssh://git@host/owner/repo(.git)
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        let (host, path) = rest.split_once('/')?;
        return Some((host.to_string(), path.to_string()));
    }
    // HTTPS form: https://host/owner/repo(.git)
    for prefix in &["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let rest = rest.strip_prefix("www.").unwrap_or(rest);
            let (host, path) = rest.split_once('/')?;
            return Some((host.to_string(), path.to_string()));
        }
    }
    None
}

fn is_known_host(host: &str) -> bool {
    matches!(host, "github.com" | "gitlab.com" | "bitbucket.org")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relative_just_now() {
        let now = Utc::now().timestamp();
        assert_eq!(format_relative(now), "just now");
        assert_eq!(format_relative(now - 30), "just now");
        // Clock skew: epoch in the future
        assert_eq!(format_relative(now + 30), "just now");
    }

    #[test]
    fn test_relative_minutes() {
        let now = Utc::now().timestamp();
        assert_eq!(format_relative(now - 60 * 5), "5m ago");
        assert_eq!(format_relative(now - 60 * 59), "59m ago");
    }

    #[test]
    fn test_relative_hours() {
        let now = Utc::now().timestamp();
        assert_eq!(format_relative(now - 60 * 60 * 2), "2h ago");
        assert_eq!(format_relative(now - 60 * 60 * 23), "23h ago");
    }

    #[test]
    fn test_relative_days() {
        let now = Utc::now().timestamp();
        assert_eq!(format_relative(now - 60 * 60 * 24 * 3), "3d ago");
        assert_eq!(format_relative(now - 60 * 60 * 24 * 29), "29d ago");
    }

    #[test]
    fn test_relative_months() {
        let now = Utc::now().timestamp();
        assert_eq!(format_relative(now - 60 * 60 * 24 * 60), "2mo ago");
    }

    #[test]
    fn test_relative_years() {
        let now = Utc::now().timestamp();
        assert_eq!(format_relative(now - 60 * 60 * 24 * 400), "1y ago");
    }

    #[test]
    fn test_url_ssh_github() {
        assert_eq!(
            remote_to_web_url("git@github.com:user/repo.git"),
            Some("https://github.com/user/repo".to_string())
        );
        assert_eq!(
            remote_to_web_url("git@github.com:user/repo"),
            Some("https://github.com/user/repo".to_string())
        );
    }

    #[test]
    fn test_url_https_github() {
        assert_eq!(
            remote_to_web_url("https://github.com/user/repo.git"),
            Some("https://github.com/user/repo".to_string())
        );
        assert_eq!(
            remote_to_web_url("https://github.com/user/repo"),
            Some("https://github.com/user/repo".to_string())
        );
    }

    #[test]
    fn test_url_gitlab() {
        assert_eq!(
            remote_to_web_url("git@gitlab.com:group/sub/repo.git"),
            Some("https://gitlab.com/group/sub/repo".to_string())
        );
    }

    #[test]
    fn test_url_bitbucket() {
        assert_eq!(
            remote_to_web_url("https://bitbucket.org/team/repo.git"),
            Some("https://bitbucket.org/team/repo".to_string())
        );
    }

    #[test]
    fn test_url_unknown_host_returns_none() {
        assert_eq!(remote_to_web_url("git@example.com:user/repo.git"), None);
        assert_eq!(
            remote_to_web_url("https://internal.company.net/user/repo.git"),
            None
        );
    }

    #[test]
    fn test_url_garbage_returns_none() {
        assert_eq!(remote_to_web_url(""), None);
        assert_eq!(remote_to_web_url("not a url"), None);
        assert_eq!(remote_to_web_url("ftp://github.com/user/repo"), None);
    }
}
