// Updates for this fork (desktop). The fork publishes every build to one rolling release, and all
// of them carry the original app's version number, so "is it newer" cannot be read off the
// version. Builds are ordered by commit time instead: the page passes the installed build's time
// on the feed address (`?build=<unix seconds>`), and the feed's `pub_date` is the published one's.

/// Release downloads of the fork, the only place its updates may come from.
/// Kept equal to `updateRepo` in build-tools/brand.json (tests/fork-update.test.ts checks it).
pub const FEED_PATH_PREFIX: &str = "/dankrick50-bit/KDENTApp/releases/download/";

/// Public half of the fork's update-signing key; equal to `updaterPubkey` in build-tools/brand.json.
/// An installer not signed with the private half is refused, whoever published it.
pub const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEIzOUNGRjY2OUJENkE2NzQKUldSMHB0YWJaditjczlFOE91dGM3K3lKNlJwZk4rd1N1eWxqc0xiTHdnWFlMaEs3MGRVVlRrdVAK";

/// The installed build's time when `endpoint` is the fork's feed, else `None`.
/// A feed address without a usable time counts as build 0, which nothing is ever newer than.
pub fn feed_build(endpoint: &tauri::Url) -> Option<i64> {
    let is_fork_feed = endpoint.scheme() == "https"
        && endpoint.host_str() == Some("github.com")
        && endpoint.path().starts_with(FEED_PATH_PREFIX);
    if !is_fork_feed {
        return None;
    }
    let build = endpoint
        .query_pairs()
        .find(|(name, _)| name == "build")
        .and_then(|(_, value)| value.parse::<i64>().ok())
        .filter(|build| *build > 0)
        .unwrap_or(0);
    Some(build)
}

/// Whether the published build should replace the installed one.
pub fn is_newer(published_at: Option<i64>, installed_build: i64) -> bool {
    // A build that could not say when it was made must not start replacing itself.
    installed_build > 0 && published_at.is_some_and(|published| published > installed_build)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> tauri::Url {
        tauri::Url::parse(raw).unwrap()
    }

    #[test]
    fn the_forks_feed_yields_the_installed_build_time() {
        let feed = url("https://github.com/dankrick50-bit/KDENTApp/releases/download/fork-latest/latest.json?build=1791756421");
        assert_eq!(feed_build(&feed), Some(1791756421));
    }

    #[test]
    fn a_feed_without_a_usable_time_is_build_zero() {
        let bare = url("https://github.com/dankrick50-bit/KDENTApp/releases/download/fork-latest/latest.json");
        assert_eq!(feed_build(&bare), Some(0));
        let junk = url("https://github.com/dankrick50-bit/KDENTApp/releases/download/fork-latest/latest.json?build=soon");
        assert_eq!(feed_build(&junk), Some(0));
        let negative = url("https://github.com/dankrick50-bit/KDENTApp/releases/download/fork-latest/latest.json?build=-5");
        assert_eq!(feed_build(&negative), Some(0));
    }

    #[test]
    fn other_addresses_are_not_the_forks_feed() {
        let original = url("https://github.com/infinitel8p/Extreme-InfiniTV/releases/latest/download/latest.json?build=5");
        assert_eq!(feed_build(&original), None);
        let plain_http = url("http://github.com/dankrick50-bit/KDENTApp/releases/download/fork-latest/latest.json?build=5");
        assert_eq!(feed_build(&plain_http), None);
        let lookalike = url("https://github.com.example.net/dankrick50-bit/KDENTApp/releases/download/fork-latest/latest.json?build=5");
        assert_eq!(feed_build(&lookalike), None);
        let other_repo = url("https://github.com/someone-else/KDENTApp/releases/download/fork-latest/latest.json?build=5");
        assert_eq!(feed_build(&other_repo), None);
    }

    #[test]
    fn only_a_later_build_replaces_the_installed_one() {
        assert!(is_newer(Some(200), 100));
        assert!(!is_newer(Some(100), 100), "the same commit built twice is the same build");
        assert!(!is_newer(Some(50), 100));
        assert!(!is_newer(None, 100));
        assert!(!is_newer(Some(200), 0), "a build that does not know itself never self-updates");
    }
}
