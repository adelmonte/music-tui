//! Podcasts. Shows are found through Apple's directory (the iTunes Search
//! API and the Apple Podcasts top chart, no key needed) and read from their
//! own RSS feeds. An episode is a `Track` whose path is its audio URL.
//! Subscriptions, how far into each episode you got, and which ones you've
//! finished are kept in `podcasts.toml`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::deezer::{Item, View};
use crate::model::Track;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Show {
    pub feed: String,
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub art: String,
}

impl Show {
    pub fn item(self) -> Item {
        Item::Show(self)
    }
}

/// The show each episode seen so far belongs to, by audio URL.
fn shows_by_episode() -> &'static std::sync::Mutex<HashMap<PathBuf, Show>> {
    static MAP: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, Show>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}

pub fn show_of(path: &std::path::Path) -> Option<Show> {
    shows_by_episode().lock().ok()?.get(path).cloned()
}

/// An episode, with when it came out (Unix seconds) for sorting.
pub struct Episode {
    pub track: Track,
    pub date: i64,
}

fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let mut resp = ureq::get(url)
        .header("User-Agent", concat!("music-tui/", env!("CARGO_PKG_VERSION")))
        .call()?;
    // Long-running shows' feeds can be tens of MB.
    Ok(resp.body_mut().with_config().limit(64 << 20).read_to_vec()?)
}

fn get_json(url: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&get_bytes(url)?)?)
}

fn shows(results: &Value) -> Vec<Item> {
    results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Show {
                feed: r["feedUrl"].as_str()?.to_string(),
                title: r["collectionName"].as_str().or(r["trackName"].as_str())?.to_string(),
                author: r["artistName"].as_str().unwrap_or("").to_string(),
                art: r["artworkUrl600"].as_str().or(r["artworkUrl100"].as_str()).unwrap_or("").to_string(),
            })
        })
        .map(Show::item)
        .collect()
}

pub fn search(q: &str) -> Vec<View> {
    let url = format!("https://itunes.apple.com/search?media=podcast&entity=podcast&limit=50&term={}", crate::spotify::url_encode(q));
    let items = get_json(&url).map(|v| shows(&v["results"])).unwrap_or_default();
    if items.is_empty() {
        return Vec::new();
    }
    vec![View { title: format!("Podcasts: {q}"), items }]
}

/// The two-letter store country from the locale, defaulting to the US.
fn country() -> String {
    std::env::var("LC_ALL")
        .ok()
        .filter(|l| !l.is_empty())
        .or_else(|| std::env::var("LANG").ok())
        .and_then(|l| l.split('.').next()?.split('_').nth(1).map(str::to_lowercase))
        .filter(|c| c.len() == 2)
        .unwrap_or_else(|| "us".into())
}

/// Apple Podcasts' top shows. The chart lists IDs, so their feeds come from
/// one lookup.
pub fn top() -> Option<View> {
    let cc = country();
    let chart = get_json(&format!("https://rss.applemarketingtools.com/api/v2/{cc}/podcasts/top/50/podcasts.json")).ok()?;
    let ids: Vec<&str> = chart["feed"]["results"].as_array()?.iter().filter_map(|r| r["id"].as_str()).collect();
    let found = get_json(&format!("https://itunes.apple.com/lookup?id={}&entity=podcast", ids.join(","))).ok()?;
    // The lookup doesn't keep the chart's order.
    let mut items = shows(&found["results"]);
    let rank: HashMap<String, usize> = found["results"]
        .as_array()?
        .iter()
        .filter_map(|r| Some((r["feedUrl"].as_str()?.to_string(), ids.iter().position(|i| r["collectionId"].as_u64().map(|c| c.to_string()).as_deref() == Some(i))?)))
        .collect();
    items.sort_by_key(|i| match i {
        Item::Show(s) => rank.get(&s.feed).copied().unwrap_or(usize::MAX),
        _ => usize::MAX,
    });
    Some(View { title: "Top podcasts".into(), items })
}

/// Read a show's feed: the show (title, author, artwork as the feed gives
/// them, falling back to `known`) and its episodes, newest first.
pub fn feed(known: &Show) -> Result<(Show, Vec<Episode>)> {
    let body = get_bytes(&known.feed)?;
    let text = String::from_utf8_lossy(&body);
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(&text, opts).context("reading the feed")?;
    let channel = doc.descendants().find(|n| n.has_tag_name("channel")).context("not an RSS feed")?;
    let child_text = |n: roxmltree::Node, name: &str| -> Option<String> {
        // The plain RSS element over a namespaced one of the same name.
        n.children()
            .filter(|c| c.tag_name().name() == name)
            .min_by_key(|c| c.tag_name().namespace().is_some())
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    };
    let itunes_image = |n: roxmltree::Node| -> Option<String> {
        n.children()
            .find(|c| c.tag_name().name() == "image" && c.attribute("href").is_some())
            .and_then(|c| c.attribute("href"))
            .map(str::to_string)
    };
    let show = Show {
        feed: known.feed.clone(),
        title: child_text(channel, "title").unwrap_or_else(|| known.title.clone()),
        author: channel
            .children()
            .find(|c| c.tag_name().name() == "author")
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| known.author.clone()),
        // Apple's copy of the artwork is small; the feed's can be huge.
        art: if known.art.is_empty() { itunes_image(channel).unwrap_or_default() } else { known.art.clone() },
    };
    let mut episodes = Vec::new();
    for item in channel.children().filter(|c| c.has_tag_name("item")) {
        let Some(url) = item
            .children()
            .find(|c| c.has_tag_name("enclosure"))
            .and_then(|e| e.attribute("url"))
            .filter(|u| u.starts_with("http"))
        else {
            continue;
        };
        let title = child_text(item, "title").unwrap_or_else(|| "Untitled episode".into());
        let date = child_text(item, "pubDate").and_then(|d| parse_date(&d)).unwrap_or(0);
        let length = item
            .children()
            .find(|c| c.tag_name().name() == "duration")
            .and_then(|c| c.text())
            .and_then(parse_duration)
            .unwrap_or(0);
        let path = PathBuf::from(url);
        crate::stream::set_art(&path, &itunes_image(item).unwrap_or_else(|| show.art.clone()));
        if let Ok(mut m) = shows_by_episode().lock() {
            m.insert(path.clone(), show.clone());
        }
        episodes.push(Episode {
            track: Track {
                id: 0,
                path,
                title,
                track_artist: show.title.clone(),
                album_artist: show.author.clone(),
                album: if date > 0 { fmt_date(date) } else { String::new() },
                year: (date > 0).then(|| civil(date).0 as i32),
                track_no: None,
                disc_no: None,
                // Unknown lengths still need one to seek and resume.
                length_secs: length.max(1),
            },
            date,
        });
    }
    episodes.sort_by_key(|e| std::cmp::Reverse(e.date));
    Ok((show, episodes))
}

/// `itunes:duration`: seconds, `MM:SS`, or `HH:MM:SS`.
fn parse_duration(s: &str) -> Option<u64> {
    let mut total = 0u64;
    for part in s.trim().split(':') {
        total = total * 60 + part.trim().split('.').next()?.parse::<u64>().ok()?;
    }
    Some(total)
}

/// RFC 2822 dates (`Wed, 06 Nov 2026 10:00:00 +0000`), to Unix seconds.
fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let parts = if parts.first()?.ends_with(',') { &parts[1..] } else { &parts[..] };
    let day: i64 = parts.first()?.parse().ok()?;
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let month = MONTHS.iter().position(|m| parts.get(1).is_some_and(|p| p.to_lowercase().starts_with(m)))? as i64 + 1;
    let mut year: i64 = parts.get(2)?.parse().ok()?;
    if year < 100 {
        year += 2000;
    }
    let hms: Vec<i64> = parts.get(3).map(|t| t.split(':').filter_map(|x| x.parse().ok()).collect()).unwrap_or_default();
    let secs = hms.first().unwrap_or(&0) * 3600 + hms.get(1).unwrap_or(&0) * 60 + hms.get(2).unwrap_or(&0);
    let offset = parts
        .get(4)
        .filter(|z| z.len() == 5 && (z.starts_with('+') || z.starts_with('-')))
        .and_then(|z| {
            let n: i64 = z[1..].parse().ok()?;
            let o = (n / 100) * 3600 + (n % 100) * 60;
            Some(if z.starts_with('-') { -o } else { o })
        })
        .unwrap_or(0);
    Some(days_from_civil(year, month, day) * 86_400 + secs - offset)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix seconds to (year, month, day).
fn civil(t: i64) -> (i64, i64, i64) {
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn fmt_date(t: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = civil(t);
    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
}

/// Subscriptions, where each episode was left off, and finished episodes.
#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub subscriptions: Vec<Show>,
    /// Seconds into each episode, by audio URL.
    #[serde(default)]
    pub progress: HashMap<String, u64>,
    #[serde(default)]
    pub played: HashSet<String>,
}

impl Store {
    pub fn load() -> Self {
        let store: Store = std::fs::read_to_string(config::podcast_store_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        store
    }

    pub fn save(&self) {
        let path = config::podcast_store_path();
        let _ = config::ensure_parent(&path);
        if let Ok(body) = toml::to_string(self) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, body).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub fn subscribed(&self, feed: &str) -> bool {
        self.subscriptions.iter().any(|s| s.feed == feed)
    }
}

/// The newest episodes across `shows`, newest first.
pub fn new_episodes(shows: &[Show]) -> View {
    let mut all: Vec<Episode> = Vec::new();
    for show in shows {
        if let Ok((_, eps)) = feed(show) {
            all.extend(eps.into_iter().take(10));
        }
    }
    all.sort_by_key(|e| std::cmp::Reverse(e.date));
    View { title: "New episodes".into(), items: all.into_iter().take(60).map(|e| Item::Track(e.track)).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_durations() {
        assert_eq!(parse_date("Thu, 01 Jan 1970 00:00:00 +0000"), Some(0));
        assert_eq!(parse_date("Fri, 06 Nov 2026 10:00:00 -0500"), parse_date("Fri, 06 Nov 2026 15:00:00 GMT"));
        assert_eq!(fmt_date(parse_date("Fri, 06 Nov 2026 10:00:00 +0000").unwrap()), "6 Nov 2026");
        assert_eq!(parse_duration("3600"), Some(3600));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("45:10"), Some(2710));
    }
}

