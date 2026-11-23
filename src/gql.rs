//! GraphQL fallback for subreddit listings Reddit refuses through its JSON APIs.
//!
//! Responses are converted to the legacy JSON listing shape so the existing
//! post parser and templates remain unchanged.

use crate::client::CLIENT;
#[cfg(not(test))]
use crate::client::OAUTH_CLIENT;
use crate::utils::{format_url, param};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::DateTime;
use log::debug;
use pulldown_cmark::{Event, Tag};
use serde_json::{json, Value};

const GQL_URL: &str = "https://gql-fed.reddit.com/";
const SUBREDDIT_FEED_HASH: &str = "97046e2a48050f10a507db354a8da60955ffccb586dcf63163bdfc6610d5cf7a";
const SORTS: [&str; 6] = ["hot", "new", "top", "rising", "controversial", "best"];

/// A single-subreddit listing rebuilt from Reddit's app GraphQL response.
/// Returns `None` for paths this deliberately small fallback does not cover.
pub(crate) async fn listing(path: &str) -> Option<Result<Value, String>> {
	let route = path.split('?').next().unwrap_or_default().trim_end_matches(".json");
	let segments: Vec<&str> = route.split('/').filter(|segment| !segment.is_empty()).collect();
	let (subreddit, sort) = match segments.as_slice() {
		["r", subreddit] => (*subreddit, "hot"),
		["r", subreddit, sort] if SORTS.contains(sort) => (*subreddit, *sort),
		_ => return None,
	};
	if subreddit.contains('+') || subreddit.to_ascii_lowercase().contains("%2b") {
		return None;
	}

	let mut variables = feed_variables(path, sort);
	variables["subredditName"] = subreddit.into();
	variables["includeSubredditInPosts"] = true.into();
	Some(feed(variables).await)
}

fn feed_variables(path: &str, sort: &str) -> Value {
	let sort = sort.to_uppercase();
	let mut variables = json!({
		"sort": sort,
		"pageSize": param(path, "limit").and_then(|limit| limit.parse::<u32>().ok()).unwrap_or(25),
	});
	if let Some(range) = param(path, "t").filter(|range| !range.is_empty() && (sort == "TOP" || sort == "CONTROVERSIAL")) {
		variables["range"] = range.to_uppercase().into();
	}
	if let Some(after) = param(path, "after").filter(|after| !after.is_empty()) {
		variables["after"] = STANDARD.encode(after).into();
	}
	variables
}

async fn feed(variables: Value) -> Result<Value, String> {
	let response = query("SubredditFeedElements", SUBREDDIT_FEED_HASH, &variables).await?;
	let connection = response["data"]
		.pointer("/postFeed/elements")
		.ok_or_else(|| format!("SubredditFeedElements returned no posts: {}", response["errors"]))?;
	Ok(post_listing(connection))
}

async fn query(operation: &str, hash: &str, variables: &Value) -> Result<Value, String> {
	let body = json!({
		"operationName": operation,
		"variables": variables,
		"extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
	});

	let mut response = Value::Null;
	for _ in 0..2 {
		let mut builder = CLIENT
			.post(GQL_URL)
			// Reddit currently answers this host with 403 over HTTP/2.
			.version(wreq::Version::HTTP_11)
			.header("X-APOLLO-OPERATION-NAME", operation)
			.header("X-APOLLO-OPERATION-ID", hash)
			.header("Accept", "multipart/mixed;deferSpec=20220824, application/graphql-response+json, application/json");
		#[cfg(test)]
		let oauth_headers = crate::oauth::android_headers().await?;
		#[cfg(not(test))]
		let oauth_headers = OAUTH_CLIENT.load_full().headers_map.clone();
		for (key, value) in &oauth_headers {
			if !key.eq_ignore_ascii_case("content-type") {
				builder = builder.header(key, value);
			}
		}

		let result = builder.json(&body).send().await.map_err(|error| error.to_string())?;
		let status = result.status();
		response = result.json().await.map_err(|error| format!("{operation} returned {status}: {error}"))?;
		if response["data"].is_null() {
			return Err(format!("{operation} returned {status}: {}", response["errors"]));
		}
		if response["errors"].is_null() {
			break;
		}
		debug!("{operation} returned partial data, retrying: {}", response["errors"]);
	}
	Ok(response)
}

fn post_listing(connection: &Value) -> Value {
	let children: Vec<Value> = connection["edges"].as_array().into_iter().flatten().filter_map(|edge| post(&edge["node"])).collect();
	json!({ "kind": "Listing", "data": { "after": after(connection), "children": children } })
}

fn after(connection: &Value) -> Option<String> {
	if connection["pageInfo"]["hasNextPage"] == false {
		return None;
	}
	let cursor = STANDARD.decode(connection["pageInfo"]["endCursor"].as_str()?).ok()?;
	String::from_utf8(cursor).ok()
}

fn post(node: &Value) -> Option<Value> {
	let node = if node["postInfo"].is_object() { &node["postInfo"] } else { node };
	if !matches!(node["__typename"].as_str(), Some("SubredditPost" | "ProfilePost")) {
		return None;
	}

	let fullname = node["id"].as_str()?;
	let content = &node["content"];
	let (flair_type, flair_richtext) = flair_parts(&node["flair"]);
	let (author_flair_type, author_flair_richtext) = flair_parts(&node["authorFlair"]);
	let thumbnail = if node["isThumbnailEnabled"] == false { &Value::Null } else { &node["thumbnail"] };
	let title = if node["title"].is_string() { &node["title"] } else { &node["postTitle"] };
	let markdown = content["markdown"].as_str().unwrap_or_default();

	let mut body_html = String::new();
	pulldown_cmark::html::push_html(&mut body_html, pulldown_cmark::Parser::new(markdown).map(safe_markdown_event));

	let mut data = json!({
		"id": fullname.strip_prefix("t3_").unwrap_or(fullname),
		"name": fullname,
		"title": title,
		"author": node["authorInfo"]["name"].as_str().or(node["authorInfo"]["displayName"].as_str()).unwrap_or("[deleted]"),
		"author_flair_type": author_flair_type,
		"author_flair_richtext": author_flair_richtext,
		"author_flair_text": node["authorFlair"]["text"],
		"author_flair_background_color": node["authorFlair"]["template"]["backgroundColor"],
		"author_flair_text_color": node["authorFlair"]["textColor"].as_str().map(str::to_lowercase),
		"subreddit": node["subreddit"]["name"].as_str().or(node["profile"]["name"].as_str()),
		"subreddit_name_prefixed": node["subreddit"]["prefixedName"].as_str().or(node["profile"]["redditorInfo"]["prefixedName"].as_str()),
		"permalink": node["permalink"],
		"url": node["url"],
		"domain": node["domain"],
		"is_self": node["isSelfPost"] == true,
		"selftext": markdown,
		"selftext_html": if content.is_object() { format!("<div class=\"md\">{body_html}</div>") } else { String::new() },
		"score": node["score"],
		"upvote_ratio": node["upvoteRatio"],
		"num_comments": node["commentCount"],
		"created_utc": timestamp(&node["createdAt"]).unwrap_or_default(),
		"edited": timestamp(&node["editedAt"]).map_or(Value::Bool(false), Value::from),
		"over_18": node["isNsfw"] == true,
		"spoiler": node["isSpoiler"] == true,
		"stickied": node["isStickied"] == true,
		"locked": node["isLocked"] == true,
		"archived": node["isArchived"] == true,
		"hide_score": node["isScoreHidden"] == true,
		"distinguished": node["distinguishedAs"].as_str().map(str::to_lowercase),
		"removed_by_category": node["removedByCategory"].as_str().map(str::to_lowercase),
		"link_flair_type": flair_type,
		"link_flair_richtext": flair_richtext,
		"link_flair_text": node["flair"]["text"],
		"link_flair_background_color": node["flair"]["template"]["backgroundColor"],
		"link_flair_text_color": node["flair"]["textColor"].as_str().map(str::to_lowercase),
		"thumbnail": thumbnail["url"].as_str().unwrap_or_default(),
		"thumbnail_width": thumbnail["dimensions"]["width"],
		"thumbnail_height": thumbnail["dimensions"]["height"],
		"post_hint": node["postHint"].as_str().or(node["media"]["typeHint"].as_str().filter(|hint| *hint == "IMAGE")).map(|hint| hint.to_lowercase().replace('_', ":")),
		"all_awardings": awards(node),
	});
	if node["isSelfPost"] != true {
		data["url_overridden_by_dest"] = node["url"].clone();
	}

	let still = &node["media"]["still"]["source"];
	if still.is_object() {
		data["preview"] = json!({ "images": [{ "source": { "url": still["url"], "width": still["dimensions"]["width"], "height": still["dimensions"]["height"] } }] });
	}

	let streaming = &node["media"]["streaming"];
	if streaming.is_object() {
		let hls_url = streaming["hlsUrl"].as_str().filter(|url| url.contains(".m3u8"));
		data["secure_media"] = json!({ "reddit_video": {
			"fallback_url": video_fallback_url(node, streaming),
			"hls_url": hls_url,
			"is_gif": streaming["isGif"] == true,
		} });
	}

	if let Some(items) = node["gallery"]["items"].as_array() {
		data["is_gallery"] = true.into();
		data["gallery_data"] = json!({ "items": items.iter().map(|item| json!({
			"media_id": item["media"]["id"], "caption": item["caption"], "outbound_url": item["outboundUrl"]
		})).collect::<Vec<_>>() });
		data["media_metadata"] = items
			.iter()
			.map(|item| {
				let media = &item["media"];
				let gif = if media["mimetype"] == "image/gif" { media["url"].clone() } else { Value::Null };
				(
					media["id"].as_str().unwrap_or_default().to_string(),
					json!({ "m": media["mimetype"], "s": { "u": media["url"], "gif": gif, "x": media["width"], "y": media["height"] } }),
				)
			})
			.collect::<serde_json::Map<_, _>>()
			.into();
	}

	if let Some(parent) = post(&node["crosspostRoot"]["post"]) {
		data["crosspost_parent_list"] = json!([parent["data"]]);
	}

	Some(json!({ "kind": "t3", "data": data }))
}

/// Reddit markdown does not allow raw HTML or active URL schemes. Preserve the
/// useful Markdown formatting without turning user text into executable HTML.
fn safe_markdown_event(event: Event<'_>) -> Event<'_> {
	match event {
		Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
		Event::Start(Tag::Link { link_type, dest_url, title, id }) => Event::Start(Tag::Link {
			link_type,
			dest_url: safe_markdown_url(dest_url),
			title,
			id,
		}),
		Event::Start(Tag::Image { link_type, dest_url, title, id }) => Event::Start(Tag::Image {
			link_type,
			dest_url: safe_markdown_url(dest_url),
			title,
			id,
		}),
		other => other,
	}
}

fn safe_markdown_url(url: pulldown_cmark::CowStr<'_>) -> pulldown_cmark::CowStr<'_> {
	let lower = url.trim_start().to_ascii_lowercase();
	if lower.starts_with("javascript:") || lower.starts_with("data:") || lower.starts_with("vbscript:") {
		"".into()
	} else {
		url
	}
}

fn flair_parts(flair: &Value) -> (&'static str, Value) {
	let richtext: Value = flair["richtext"].as_str().and_then(|value| serde_json::from_str(value).ok()).unwrap_or_default();
	let kind = if richtext.as_array().is_some_and(|parts| !parts.is_empty()) {
		"richtext"
	} else if flair["text"].as_str().is_some_and(|text| !text.is_empty()) {
		"text"
	} else {
		""
	};
	(kind, richtext)
}

fn awards(node: &Value) -> Vec<Value> {
	node["awardings"]
		.as_array()
		.into_iter()
		.flatten()
		.map(|award| {
			json!({
				"name": award["award"]["name"],
				"count": award["total"],
				"resized_icons": [{ "url": award["award"]["static_icon_16"]["url"] }],
			})
		})
		.collect()
}

fn video_fallback_url(node: &Value, streaming: &Value) -> String {
	if let Some(url) = streaming["fallbackUrl"].as_str() {
		return format_url(url);
	}
	let dimensions = &streaming["dimensions"];
	let size = dimensions["width"].as_u64().unwrap_or_default().min(dimensions["height"].as_u64().unwrap_or_default());
	let rendition = [1080, 720, 480, 360, 240].into_iter().find(|rendition| *rendition <= size).unwrap_or(240);
	format!("{}/CMAF_{rendition}.mp4?source=fallback", node["url"].as_str().unwrap_or_default().trim_end_matches('/'))
}

fn timestamp(value: &Value) -> Option<f64> {
	DateTime::parse_from_str(value.as_str()?, "%Y-%m-%dT%H:%M:%S%.f%z").ok().map(|time| time.timestamp() as f64)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn supports_only_single_subreddit_listings() {
		for path in ["/r/rust/new.json?raw_json=1", "/r/rust/rising.json", "/r/rust.json"] {
			let route = path.split('?').next().unwrap().trim_end_matches(".json");
			let segments: Vec<&str> = route.split('/').filter(|segment| !segment.is_empty()).collect();
			assert!(matches!(segments.as_slice(), ["r", _] | ["r", _, _]));
		}
	}

	#[test]
	fn markdown_fallback_escapes_html_and_active_links() {
		let mut html = String::new();
		pulldown_cmark::html::push_html(
			&mut html,
			pulldown_cmark::Parser::new("<script>alert(1)</script> [x](javascript:alert(2))").map(safe_markdown_event),
		);
		assert!(!html.contains("<script>"), "{html}");
		assert!(!html.contains("href=\"javascript:"), "{html}");
	}

	#[tokio::test]
	#[ignore = "calls Reddit"]
	async fn live_new_listing() {
		let _ = pretty_env_logger::try_init();
		let response = listing("/r/rust/new.json?raw_json=1").await.unwrap().unwrap();
		assert!(!response["data"]["children"].as_array().unwrap().is_empty());
	}
}
