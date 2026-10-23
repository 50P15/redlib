# Anonymous comments 403: investigation notes

On 22 October 2026, Redlib could still load subreddit listings but every post
page failed. Anonymous OAuth setup also continued to work. The failing request
was the comments JSON endpoint, which returned a 403 HTML page instead of JSON.

This change stops using that endpoint for threads. It reads comments from the
same Shreddit endpoints used by Reddit's logged-out website and continues to use
`/by_id` for post metadata.

## Background

The breakage and the short-lived path-encoding workaround are documented in
[#591](https://github.com/redlib-org/redlib/issues/591),
[#598](https://github.com/redlib-org/redlib/issues/598), and
[#446](https://github.com/redlib-org/redlib/issues/446). The workaround from
[PR #592](https://github.com/redlib-org/redlib/pull/592) encoded the separator
before `comments`. It worked on 21 October and was blocked the following day.
That made it useful as a diagnostic, but not worth extending with more URL
encoding tricks.

The Shreddit endpoint and parser work started from these AGPL-3.0-only fork
commits by polak14:

- [`6aed00a7`](https://github.com/polak14/redlib/commit/6aed00a7cadba3fbf24979322936d92859968ee8)
- [`0baefabf`](https://github.com/polak14/redlib/commit/0baefabf4e806006e62c30a3b8766fa458bf08d2)
- [`4055fda1`](https://github.com/polak14/redlib/commit/4055fda1a5355f1507b23108795d827a16794b58)

The checkout used here was Redlib v0.40.0 at
`ba116c1186f6b0f9d45f0ae50c827e13eb2f5ea2`. The running Docker instance was
left alone throughout the work.

## What I found

Plain `curl` was not a useful control on this connection: Reddit returned 403
for listings as well as threads. Redlib's `wreq` client could load listings, so
all comparisons below used that client and a fresh anonymous identity.

The old comments JSON route still returned 403. The logged-out web endpoints
worked:

```text
GET  /svc/shreddit/comments/r/{subreddit}/t3_{post}?sort={sort}
GET  /svc/shreddit/comment/t1_{comment}?depth=0&subredditName={subreddit}
POST /svc/shreddit/more-comments/{subreddit}/t3_{post}
```

Results from the first isolated run were:

| Check | Result |
| --- | --- |
| Anonymous OAuth listing | Loaded normally |
| Legacy comments JSON | 403 with an HTML response |
| Supplied Northern Ireland post | Loaded correctly with no comments |
| Large `r/mildlyinfuriating` post | 25 comments and a continuation |
| `r/politics` post | 25 comments |
| `sort=new` | 25 comments |
| Continuation | 10 more comments |
| Focused comment | Target loaded and was highlighted |

The raw summary is in
[`investigation-results.json`](investigation-results.json). It does not contain
cookies, tokens, headers, comment bodies, device IDs, or tracking IDs.

I also looked at GraphQL, `/duplicates`, and `/by_id`. I did not find a current,
known-good anonymous GraphQL comments operation before Shreddit passed the
required tests. `/duplicates` returns related posts rather than a comment tree,
and `/by_id` is useful for the post itself but not its comments. I stopped there
instead of adding unproven fallbacks.

## Implementation

Thread fetching now lives in `src/thread.rs`; the generic listing path is
unchanged. The adapter turns Shreddit markup into Redlib's existing post and
comment structures, including parent relationships and continuation state.

A few constraints are deliberate:

- Requests only go to fixed Reddit hosts and redirects are disabled.
- Requests time out after 20 seconds and responses are capped at 8 MiB.
- A comments 403 does not cause an OAuth token refresh.
- Continuation data is opaque local state tied to the post and sort order. It
  cannot be used to make Redlib request an arbitrary URL.
- Parsed comment HTML is rebuilt from an allowlist. Scripts, event handlers,
  unsafe URL schemes, tracking elements, and third-party media loads are
  discarded.
- Successful thread responses are cached for 30 seconds. Errors are not cached
  as empty discussions.
- Tokens, device IDs, session headers, and cookies are no longer printed by
  OAuth info or trace logging.

The code reports restricted, missing, rate-limited, blocked, malformed, and
transport failures separately. This avoids presenting Reddit's HTML block page
as a JSON parsing error.

## Testing

The source passed:

```sh
cargo fmt --all -- --check
cargo test --locked --offline thread::tests
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --locked --offline --release --bins
```

The seven targeted tests cover nesting, empty threads, removed comments,
focused comments, continuations, malformed input, and HTML sanitizing.

I ran the release probe four times with a new anonymous identity each time. All
four listing controls and all 12 thread checks passed. I then ran the release
server on `127.0.0.1:18081` and checked listing-to-thread navigation, an empty
thread, a populated thread, `sort=new`, a focused comment, continuation, comment
search, and user filtering. Reusing a continuation with a different sort was
rejected locally as expected. The test server was stopped afterward.

This was not a six-hour soak test. The checks ran between the evening of 22
October and the morning of 23 October, but they were separate runs rather than
regular samples from one continuously running binary.

## Reproducing the live check

Build and run the probe with:

```sh
cargo run --locked --bin redlib-probe
```

It first checks that listings work, then tests threads one at a time with a
two-second gap. It prints JSON Lines with no credentials or response bodies.
Local Redlib thread paths can be supplied as arguments to replace its default
test set.

## If Reddit changes the markup

First run the probe. If its listing control also fails, do not assume the
problem is specific to comments. If listings work but thread parsing fails,
capture the smallest sanitized Shreddit fragment that reproduces the change,
update the selectors in `src/thread.rs`, and add that fragment as a test case.
Re-run the nesting, sort, focus, continuation, empty-thread, and sanitizer tests
before trying the live endpoint again.

Do not commit cookies, tokens, live continuation cursors, personal data, or
tracking identifiers.

## Limitations

This is easier to maintain than another routing bypass, but it is not
future-proof in the literal sense. Reddit owns the logged-out endpoints and can
change their markup, require different anonymous state, or remove public access
altogether. The goal here is to keep the Reddit-specific code contained, fail
clearly, and make the next change diagnosable—not to promise that Reddit cannot
break it.
