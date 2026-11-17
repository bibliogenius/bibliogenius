//! What a lookup is doing, for the app to say while the user waits: a
//! publisher's site asking 10 s between two requests makes a lookup last
//! half a minute, most of it spent waiting.
//!
//! Steps are reported to whoever runs the lookup inside [`reporting_to`]; a
//! lookup run outside it (the maintenance tool, tests) reports to no one.
//! The reporter is task-local rather than a parameter: the steps come from
//! every layer (recipes, robots.txt, the pause between two requests) and the
//! fetcher is shared by every lookup, which each have their own reporter.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// One step of a lookup. `site` is the host as the table names it, `www.`
/// left out (`gallimard.fr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupStep {
    /// Reading the site's robots.txt.
    Rules { site: String },
    /// Trying the book's page at an address built from its ISBN and title:
    /// `attempt` of at most `of`.
    ProductPage { site: String, attempt: u32, of: u32 },
    /// Asking the site's own search.
    Search { site: String },
    /// Opening one of the search's results: `attempt` of at most `of`.
    SearchResult { site: String, attempt: u32, of: u32 },
    /// Reading the site map, or opening the pages it lists.
    SiteMap { site: String },
    /// Waiting before the next request, as the site asks: `seconds` left.
    Waiting { site: String, seconds: u32 },
    /// The book's page is found: fetching its cover.
    Cover { site: String },
}

/// Where a lookup's steps go.
pub type Reporter = Arc<dyn Fn(LookupStep) + Send + Sync>;

/// A wait shorter than this is not worth saying.
pub const WAIT_WORTH_SAYING: Duration = Duration::from_secs(1);

tokio::task_local! {
    static REPORTER: Reporter;
}

/// Run `lookup`, its steps reported to `reporter`.
pub async fn reporting_to<F: Future>(reporter: Reporter, lookup: F) -> F::Output {
    REPORTER.scope(reporter, lookup).await
}

/// Say `step` to the lookup's reporter, if it has one.
pub fn report(step: LookupStep) {
    let _ = REPORTER.try_with(|reporter| reporter(step));
}

/// The seconds a wait of `wait` is announced as: rounded up, so the count
/// never reaches 0 before the request goes out.
pub fn seconds_of(wait: Duration) -> u32 {
    u32::try_from(wait.as_millis().div_ceil(1000)).unwrap_or(u32::MAX)
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A reporter that keeps the steps, and what it kept.
    pub fn recorder() -> (Reporter, Arc<Mutex<Vec<LookupStep>>>) {
        let steps = Arc::new(Mutex::new(Vec::new()));
        let kept = steps.clone();
        let reporter: Reporter = Arc::new(move |step| kept.lock().unwrap().push(step));
        (reporter, steps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn steps_go_to_the_lookup_s_own_reporter_only() {
        let (reporter, steps) = testing::recorder();
        let site = || "gallimard.fr".to_owned();
        reporting_to(reporter, async {
            report(LookupStep::Search { site: site() });
        })
        .await;
        // Outside a lookup: nobody listens, nothing breaks.
        report(LookupStep::Cover { site: site() });
        assert_eq!(
            *steps.lock().unwrap(),
            [LookupStep::Search { site: site() }]
        );
    }

    #[test]
    fn a_wait_is_rounded_up_to_the_second() {
        assert_eq!(seconds_of(Duration::from_millis(9_001)), 10);
        assert_eq!(seconds_of(Duration::from_secs(10)), 10);
        assert_eq!(seconds_of(Duration::from_millis(1)), 1);
    }
}
