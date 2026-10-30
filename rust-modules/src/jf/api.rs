//! The Jellyfin operations.
//!
//! `Client::jf()` hands one of these out when the client's origin is a Jellyfin seat
//! ([`super::seat`]). The list stores ask it for Jellyfin results directly — [`JfShelf`]s,
//! [`JfPage`]s, [`JfSearch`] and `BaseItemDto` rows — and build their own screen rows from them.
//! The detail, playback and timeline ops still answer in the catalog record types the player and
//! the route planner read, through `catalog::Client`'s delegation. Requests go through
//! `Client::jf_send` — the one [`crate::http`] door, origin, resolve pin and credential gate —
//! with the `Authorization: MediaBrowser …` header.
use super::models::*;
use super::{convert, ids, seat, url};
use crate::http::Method;
use crate::catalog::collections::CollectionOutcome;
use crate::catalog::{
    Client, Hub, LibrarySection, MediaContainer, Meta, MetaType, Metadata, SectionQuery,
    SortOption, Tag,
};
use serde::de::DeserializeOwned;

/// One Home or Library shelf as Jellyfin answered it.
#[derive(Debug, Default, Clone)]
pub struct JfShelf {
    /// The locale-independent shelf id Home and the Library key titles and focus memory by
    /// (`home.movies.recent`, `tv.ondeck.{section}`, …).
    pub identifier: String,
    pub title: String,
    pub key: String,
    /// The library the shelf lists, 0 for none; and that library's name.
    pub section: i64,
    pub library: String,
    pub items: Vec<BaseItemDto>,
}

/// One page of a library listing.
#[derive(Default)]
pub struct JfPage {
    pub items: Vec<BaseItemDto>,
    pub total: i64,
    pub start: i64,
    /// The sort menu, when the query asked for it.
    pub meta: Option<Meta>,
}

/// A search answer: title hits per [`SEARCH_GROUPS`] type, in that order, and person hits.
#[derive(Debug, Default, Clone)]
pub struct JfSearch {
    pub groups: Vec<(&'static str, Vec<BaseItemDto>)>,
    pub people: Vec<BaseItemDto>,
}

/// `(Jellyfin Type, shelf kind, heading)` for each title group a search returns.
pub const SEARCH_GROUPS: [(&str, &str, &str); 4] = [
    ("Movie", "movie", "Movies"),
    ("Series", "show", "Shows"),
    ("Episode", "episode", "Episodes"),
    ("BoxSet", "collection", "Collections"),
];

/// `Fields=` for list endpoints: what a poster grid and a shelf draw, without the per-item
/// `MediaSources` a detail page needs (those make a 50-row page ~40x larger).
const LIST_FIELDS: &str =
    "PrimaryImageAspectRatio,Overview,Genres,DateCreated,ChildCount,RecursiveItemCount,ProviderIds,Taglines";
/// …and for rows that may be PLAYED straight from the list (episodes, Up Next, Continue
/// Watching): those need their media sources so the route can pick a part without a detail read.
const PLAYABLE_FIELDS: &str =
    "PrimaryImageAspectRatio,Overview,Genres,DateCreated,ChildCount,RecursiveItemCount,ProviderIds,Taglines,MediaSources,Chapters";

/// Percent-encoded query assembly — the JF twin of `plex::QueryBuilder`, over the same encoder.
pub(crate) struct Q {
    path: String,
    parts: Vec<String>,
}

impl Q {
    pub(crate) fn new(path: impl Into<String>) -> Self {
        Self { path: path.into(), parts: Vec::new() }
    }
    pub(crate) fn s(mut self, k: &str, v: &str) -> Self {
        if !v.is_empty() {
            self.parts.push(format!("{k}={}", crate::catalog::urlenc_str(v)));
        }
        self
    }
    pub(crate) fn i(mut self, k: &str, v: i64) -> Self {
        self.parts.push(format!("{k}={v}"));
        self
    }
    pub(crate) fn b(self, k: &str, v: bool) -> Self {
        self.s(k, if v { "true" } else { "false" })
    }
    pub(crate) fn build(self) -> String {
        if self.parts.is_empty() {
            self.path
        } else {
            let sep = if self.path.contains('?') { '&' } else { '?' };
            format!("{}{sep}{}", self.path, self.parts.join("&"))
        }
    }
}

/// The Plex `sort` expression (`titleSort:asc`, `addedAt:desc`, `a:desc,b`) → `(SortBy, SortOrder)`.
pub(crate) fn sort_by(plex_sort: &str) -> (String, &'static str) {
    let mut fields = Vec::new();
    let mut order = "Ascending";
    for (i, part) in plex_sort.split(',').filter(|p| !p.is_empty()).enumerate() {
        let (key, dir) = part.split_once(':').unwrap_or((part, "asc"));
        if i == 0 && dir == "desc" {
            order = "Descending";
        }
        let mapped: &[&str] = match key {
            "titleSort" | "title" => &["SortName"],
            "addedAt" => &["DateCreated"],
            "originallyAvailableAt" | "year" => &["PremiereDate", "ProductionYear"],
            "audienceRating" => &["CommunityRating"],
            "rating" => &["CriticRating"],
            "lastViewedAt" => &["DatePlayed"],
            "viewCount" => &["PlayCount"],
            "duration" => &["Runtime"],
            "random" => &["Random"],
            "showOrder" => &["SeriesSortName", "ParentIndexNumber", "IndexNumber"],
            "seasonOrder" => &["SeriesSortName", "IndexNumber"],
            _ => &[],
        };
        for m in mapped {
            if !fields.contains(m) {
                fields.push(*m);
            }
        }
    }
    if fields.is_empty() {
        fields.push("SortName");
    }
    if !fields.contains(&"SortName") && !fields.contains(&"Random") {
        fields.push("SortName");
    }
    (fields.join(","), order)
}

fn sort_option(key: &str, title: &str, default_direction: &str) -> SortOption {
    SortOption { key: key.into(), desc_key: String::new(), default_direction: default_direction.into(), title: title.into() }
}

/// The sort menu a section advertises for `jf_type` — the server-driven `Meta` PMS sends with
/// `includeMeta=1`, synthesized from what `/Items` can actually order by.
fn sort_menu(jf_type: &str) -> Meta {
    let mut sort = match jf_type {
        "Episode" => vec![SortOption {
            key: "showOrder".into(),
            desc_key: "showOrder:desc".into(),
            default_direction: "asc".into(),
            title: "Show".into(),
        }],
        "Season" => vec![sort_option("seasonOrder", "Show", "asc")],
        _ => Vec::new(),
    };
    sort.extend([
        sort_option("titleSort", "Title", "asc"),
        sort_option("addedAt", "Date Added", "desc"),
        sort_option("originallyAvailableAt", "Release Date", "desc"),
        sort_option("audienceRating", "Rating", "desc"),
        sort_option("lastViewedAt", "Last Played", "desc"),
        sort_option("duration", "Duration", "desc"),
    ]);
    Meta { types: vec![MetaType { active: 1, sort }] }
}

pub struct Jf<'a> {
    c: &'a Client,
    /// The DeviceId basis for a sign-in, before the origin has a seat to hold it.
    device_user: Option<String>,
}

impl<'a> Jf<'a> {
    pub(crate) fn new(c: &'a Client) -> Self {
        Self { c, device_user: None }
    }

    pub(crate) fn for_sign_in(c: &'a Client, device_user: &str) -> Self {
        Self { c, device_user: Some(device_user.to_string()) }
    }

    fn seat(&self) -> seat::Seat {
        seat::get(self.c.origin()).unwrap_or_default()
    }

    pub(crate) fn origin(&self) -> &crate::catalog::Origin {
        self.c.origin()
    }

    pub(crate) fn token(&self) -> String {
        self.c.current_token()
    }

    /// The four device facts plus this user's DeviceId — see [`url::device_id`].
    pub(crate) fn identity(&self) -> url::DeviceIdentity {
        let s = self.seat();
        url::DeviceIdentity {
            client: crate::catalog::identity::PRODUCT.to_string(),
            device: crate::catalog::identity::device_name().to_string(),
            device_id: url::device_id(self.c.client_id(), self.device_user.as_deref().unwrap_or(&s.device_user)),
            version: crate::catalog::identity::VERSION.to_string(),
        }
    }

    pub(crate) fn auth_header(&self) -> String {
        url::authorization(&self.identity(), &self.c.current_token())
    }

    fn send(&self, path: &str, method: Method, body: Option<&[u8]>) -> Option<crate::http::Reply> {
        let auth = self.auth_header();
        let mut headers = vec![crate::http::ACCEPT_JSON, auth.as_str()];
        if body.is_some() {
            headers.push("Content-Type: application/json");
        }
        self.c.jf_send(path, method, &headers, body)
    }

    pub(crate) fn status(&self, path: &str, method: Method, body: Option<&[u8]>) -> Option<i32> {
        self.send(path, method, body).map(|r| r.status)
    }

    fn ok(&self, path: &str, method: Method, body: Option<&[u8]>) -> bool {
        self.send(path, method, body).is_some_and(|r| r.ok())
    }

    /// Worker-only bounded preview request. Header authentication also works on private libraries.
    pub(crate) fn preview_bytes(&self, path: &str, max: usize) -> crate::catalog::ArtFetch {
        self.c.jf_preview_get(path, &[&self.auth_header()], max)
    }

    pub(crate) fn get<T: DeserializeOwned>(&self, path: &str) -> Option<T> {
        let r = self.send(path, Method::Get, None)?;
        if !r.ok() {
            return None;
        }
        parse(path, &r.body)
    }

    fn get_status<T: DeserializeOwned>(&self, path: &str) -> (Option<i32>, Option<T>) {
        match self.send(path, Method::Get, None) {
            None => (None, None),
            Some(r) if r.ok() => (Some(r.status), parse(path, &r.body)),
            Some(r) => (Some(r.status), None),
        }
    }

    pub(crate) fn post<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Option<T> {
        let bytes = serde_json::to_vec(body).ok()?;
        let r = self.send(path, Method::Post, Some(&bytes))?;
        if !r.ok() {
            return None;
        }
        parse(path, &r.body)
    }

    pub(crate) fn post_ok(&self, path: &str, body: Option<&impl serde::Serialize>) -> bool {
        match body {
            Some(b) => match serde_json::to_vec(b) {
                Ok(bytes) => self.ok(path, Method::Post, Some(&bytes)),
                Err(_) => false,
            },
            None => self.ok(path, Method::Post, None),
        }
    }

    /// The signed-in user's id, learned once per seat from `GET /Users/Me`.
    pub(crate) fn user_id(&self) -> Option<String> {
        let s = self.seat();
        if !s.user_id.is_empty() {
            return Some(s.user_id);
        }
        let me: UserDto = self.get("/Users/Me")?;
        if me.id.is_empty() {
            return None;
        }
        let id = me.id.clone();
        seat::update(self.c.origin(), |s| {
            s.user_id = me.id;
            if s.user_name.is_empty() {
                s.user_name = me.name;
            }
            if s.server_id.is_empty() {
                s.server_id = me.server_id;
            }
        });
        Some(id)
    }

    fn items(&self, q: Q) -> Option<QueryResult<BaseItemDto>> {
        let q = match self.user_id() {
            Some(uid) => q.s("userId", &uid),
            None => q,
        };
        self.get(&q.build())
    }

    fn items_container(&self, q: Q, section_id: i64) -> Option<MediaContainer> {
        let r = self.items(q)?;
        let rows = r.items.iter().map(|i| convert::item(i, section_id)).collect();
        Some(convert::container(rows, r.total_record_count, r.start_index))
    }

    /// The GUID behind a key. A key PERSISTED by an earlier run (the cold-open Home cache, a resume
    /// target, a dev `play` trigger) was never minted in this process, and the mapping is one-way;
    /// on such a miss one ids-only sweep of the user's items re-mints every key. Throttled per
    /// origin, so a key that is genuinely gone costs one sweep a minute and not one per lookup.
    pub(crate) fn guid(&self, rk: &str) -> Option<String> {
        if let Some(g) = ids::guid_of_key(rk) {
            return Some(g);
        }
        if rk.is_empty() || !rk.bytes().all(|b| b.is_ascii_digit()) || !self.sweep_due() {
            return None;
        }
        let q = Q::new("/Items")
            .b("Recursive", true)
            .s("IncludeItemTypes", "Movie,Series,Season,Episode,BoxSet,Video,MusicVideo")
            .b("EnableImages", false)
            .b("EnableUserData", false)
            .b("EnableTotalRecordCount", false)
            .s("Fields", "");
        let r = self.items(q)?;
        for i in &r.items {
            ids::intern(&i.id);
        }
        ids::guid_of_key(rk)
    }

    fn sweep_due(&self) -> bool {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        use std::time::{Duration, Instant};
        static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
        let Ok(mut m) = LAST.get_or_init(Default::default).lock() else { return false };
        let key = self.origin().base().to_ascii_lowercase();
        let now = Instant::now();
        if m.get(&key).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(60)) {
            return false;
        }
        m.insert(key, now);
        true
    }

    // ---- server ---------------------------------------------------------------------------

    pub(crate) fn public_info(&self) -> Option<PublicSystemInfo> {
        let info: PublicSystemInfo = self.get("/System/Info/Public")?;
        seat::update(self.c.origin(), |s| {
            s.server_id = info.id.clone();
            s.server_name = info.server_name.clone();
            s.server_version = info.version.clone();
        });
        Some(info)
    }

    pub fn friendly_name(&self) -> Option<String> {
        self.public_info().map(|i| i.server_name).filter(|s| !s.is_empty())
    }

    pub fn machine_identity(&self) -> Option<String> {
        self.public_info().map(|i| ids::normalize(&i.id)).filter(|s| !s.is_empty())
    }

    /// The "server root" read `serverinfo` makes: version only (Jellyfin has no subscription).
    pub fn server_root(&self) -> Option<MediaContainer> {
        let i = self.public_info()?;
        Some(MediaContainer { friendly_name: i.server_name, version: i.version,
            machine_identifier: ids::normalize(&i.id), ..Default::default() })
    }

    // ---- library --------------------------------------------------------------------------

    pub fn sections(&self) -> Option<MediaContainer> {
        let uid = self.user_id()?;
        let views: QueryResult<BaseItemDto> = self.get(&Q::new("/UserViews").s("userId", &uid).build())?;
        let directory: Vec<LibrarySection> = views.items.iter().filter_map(convert::section).collect();
        Some(MediaContainer { size: directory.len() as i64, directory, ..Default::default() })
    }

    /// Which `IncludeItemTypes` a section listing names: the explicit `type=` filter, else the
    /// section's primary type (Movie for a movie library, Series for TV).
    fn section_type(&self, section_guid: &str, filters: &[(String, String)]) -> &'static str {
        if let Some(t) = filters.iter().find(|(k, _)| k == "type").and_then(|(_, v)| v.parse().ok())
            .and_then(convert::jf_type_of_number)
        {
            return t;
        }
        match self.view_kind(section_guid).as_deref() {
            Some("tvshows") => "Series",
            _ => "Movie",
        }
    }

    fn view_kind(&self, section_guid: &str) -> Option<String> {
        let uid = self.user_id()?;
        let views: QueryResult<BaseItemDto> = self.get(&Q::new("/UserViews").s("userId", &uid).build())?;
        views.items.into_iter().find(|v| ids::normalize(&v.id) == section_guid).and_then(|v| v.collection_type)
    }

    /// One page of a library grid: `/Items` under the library, typed, sorted, filtered.
    pub fn section_page(&self, q: &SectionQuery) -> Option<JfPage> {
        let section_guid = ids::guid_of(q.section_key)?;
        let jf_type = self.section_type(&section_guid, q.filters);
        let (sort, order) = sort_by(if q.sort.is_empty() {
            match jf_type { "Episode" => "showOrder", "Season" => "seasonOrder", _ => "titleSort" }
        } else {
            q.sort
        });
        let mut b = Q::new("/Items")
            .s("IncludeItemTypes", jf_type)
            .b("Recursive", true)
            .s("SortBy", &sort)
            .s("SortOrder", order)
            .s("Fields", if jf_type == "Episode" { PLAYABLE_FIELDS } else { LIST_FIELDS })
            .s("ImageTypeLimit", "1")
            .s("EnableImageTypes", "Primary,Backdrop,Logo,Thumb")
            .b("EnableTotalRecordCount", true)
            .i("StartIndex", q.start)
            .i("Limit", q.size);
        if jf_type != "BoxSet" {
            b = b.s("ParentId", &section_guid);
        }
        for (k, v) in q.filters {
            b = match k.as_str() {
                "unwatched" | "unwatchedLeaves" if v == "1" => b.s("Filters", "IsUnplayed"),
                "genre" => match v.parse::<i64>().ok().and_then(ids::guid_of) {
                    Some(g) => b.s("GenreIds", &g),
                    None => b.s("Genres", v),
                },
                "firstCharacter" => b.s("NameStartsWith", v),
                _ => b,
            };
        }
        let r = self.items(b)?;
        Some(JfPage {
            items: r.items,
            total: r.total_record_count,
            start: r.start_index,
            meta: q.include_meta.then(|| sort_menu(jf_type)),
        })
    }

    pub fn section_directory(&self, section_key: i64, directory: &str, metadata_type: Option<i64>) -> Option<MediaContainer> {
        let section_guid = ids::guid_of(section_key)?;
        let jf_type = metadata_type.and_then(convert::jf_type_of_number)
            .unwrap_or_else(|| self.section_type(&section_guid, &[]));
        let rows: Vec<LibrarySection> = match directory {
            "genre" => {
                let f: QueryFilters = self.get(&Q::new("/Items/Filters2")
                    .s("userId", &self.user_id()?)
                    .s("ParentId", &section_guid)
                    .s("IncludeItemTypes", jf_type)
                    .build())?;
                f.genres.iter().map(|g| LibrarySection {
                    key: ids::intern(&g.id).to_string(),
                    kind: "genre".into(),
                    title: g.name.clone(),
                    size: 0,
                }).collect()
            }
            "firstCharacter" => {
                // One sorted read of every name, grouped by its first character — the server's own
                // SortName order, which is what the grid scrolls through.
                let r = self.items(Q::new("/Items")
                    .s("ParentId", &section_guid)
                    .s("IncludeItemTypes", jf_type)
                    .b("Recursive", true)
                    .s("SortBy", "SortName")
                    .s("Fields", "SortName")
                    .b("EnableImages", false)
                    .b("EnableUserData", false))?;
                let mut out: Vec<LibrarySection> = Vec::new();
                for it in &r.items {
                    let name = it.sort_name.as_deref().unwrap_or(&it.name);
                    let c = name.chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or('#');
                    let key = if c.is_ascii_alphabetic() { c.to_string() } else { "#".to_string() };
                    match out.last_mut() {
                        Some(last) if last.key == key => last.size += 1,
                        _ => out.push(LibrarySection { key: key.clone(), kind: String::new(), title: key, size: 1 }),
                    }
                }
                out
            }
            _ => Vec::new(),
        };
        Some(MediaContainer { size: rows.len() as i64, directory: rows, ..Default::default() })
    }

    /// The full item, with its markers (MediaSegments) and — for a show — the next episode.
    pub fn metadata(&self, rk: &str) -> Option<Metadata> {
        let guid = self.guid(rk)?;
        let uid = self.user_id()?;
        let it: BaseItemDto = self.get(&Q::new(format!("/Items/{guid}")).s("userId", &uid).build())?;
        let mut m = convert::item(&it, 0);
        if matches!(it.kind.as_str(), "Movie" | "Episode" | "Video") {
            if let Some(segs) = self.get::<QueryResult<MediaSegmentDto>>(&format!("/MediaSegments/{guid}")) {
                m.marker = convert::markers(&segs.items, m.duration);
            }
        }
        if it.kind == "Series" {
            if let Some(next) = self.items(Q::new("/Shows/NextUp").s("SeriesId", &guid).i("Limit", 1)
                .s("Fields", PLAYABLE_FIELDS).b("EnableResumable", true))
                .and_then(|r| r.items.into_iter().next())
            {
                m.on_deck = Some(crate::catalog::OnDeckHub { metadata: Some(Box::new(convert::item(&next, 0))) });
            }
        }
        Some(m)
    }

    pub fn extras(&self, rk: &str) -> Option<MediaContainer> {
        let guid = self.guid(rk)?;
        let uid = self.user_id()?;
        let rows: Vec<BaseItemDto> = self.get(&Q::new(format!("/Items/{guid}/SpecialFeatures")).s("userId", &uid).build())?;
        let mut items: Vec<Metadata> = rows.iter().map(|i| convert::item(i, 0)).collect();
        // A local trailer is listed separately from special features on Jellyfin.
        if let Some(trailers) = self.get::<Vec<BaseItemDto>>(&Q::new(format!("/Items/{guid}/LocalTrailers")).s("userId", &uid).build()) {
            for t in &trailers {
                let mut m = convert::item(t, 0);
                m.kind = "clip".into();
                m.subtype = "trailer".into();
                m.extra_type = 1;
                items.insert(0, m);
            }
        }
        let n = items.len() as i64;
        Some(convert::container(items, n, 0))
    }

    pub fn metadata_many(&self, rks: &[&str]) -> Option<MediaContainer> {
        let guids: Vec<String> = rks.iter().filter_map(|rk| self.guid(rk)).collect();
        if guids.is_empty() {
            return Some(MediaContainer::default());
        }
        let r = self.items(Q::new("/Items").s("Ids", &guids.join(",")).s("Fields", "People"))?;
        // request order, as PMS answers
        let mut rows: Vec<Metadata> = Vec::with_capacity(r.items.len());
        for g in &guids {
            if let Some(it) = r.items.iter().find(|i| ids::normalize(&i.id) == *g) {
                rows.push(convert::item(it, 0));
            }
        }
        let n = rows.len() as i64;
        Some(convert::container(rows, n, 0))
    }

    pub fn children(&self, rk: &str) -> Option<MediaContainer> {
        let guid = self.guid(rk)?;
        let uid = self.user_id()?;
        let it: BaseItemDto = self.get(&Q::new(format!("/Items/{guid}")).s("userId", &uid).s("Fields", "ChildCount").build())?;
        let q = match it.kind.as_str() {
            "Series" => Q::new(format!("/Shows/{guid}/Seasons")).s("Fields", LIST_FIELDS),
            "Season" => Q::new(format!("/Shows/{}/Episodes", ids::normalize(it.series_id.as_deref().unwrap_or(""))))
                .s("SeasonId", &guid).s("Fields", PLAYABLE_FIELDS),
            _ => Q::new("/Items").s("ParentId", &guid).s("Fields", LIST_FIELDS).s("SortBy", "SortName"),
        };
        self.items_container(q.s("ImageTypeLimit", "1").s("EnableImageTypes", "Primary,Backdrop,Logo,Thumb"), 0)
    }

    pub fn all_leaves(&self, rk: &str) -> Option<MediaContainer> {
        let guid = self.guid(rk)?;
        self.items_container(Q::new(format!("/Shows/{guid}/Episodes")).s("Fields", PLAYABLE_FIELDS), 0)
    }

    /// "More Like This": `/Items/{id}/Similar`.
    pub fn similar_items(&self, rk: &str) -> Option<Vec<BaseItemDto>> {
        let guid = self.guid(rk)?;
        Some(self.items(Q::new(format!("/Items/{guid}/Similar")).i("Limit", 20).s("Fields", LIST_FIELDS))?.items)
    }

    /// A person's filmography: every movie and series they are credited in, newest first.
    pub fn person_items(&self, person_id: &str) -> Option<Vec<BaseItemDto>> {
        let guid = if person_id.bytes().all(|b| b.is_ascii_digit()) {
            self.guid(person_id)?
        } else {
            ids::normalize(person_id)
        };
        Some(self.items(Q::new("/Items").s("PersonIds", &guid).b("Recursive", true)
            .s("IncludeItemTypes", "Movie,Series").s("SortBy", "PremiereDate,SortName")
            .s("SortOrder", "Descending").s("Fields", LIST_FIELDS))?.items)
    }

    /// `find_by_guid`: does this server hold the item a portable guid names (`imdb://tt…`)?
    pub fn find_by_guid(&self, guid: &str) -> Option<MediaContainer> {
        let (scheme, id) = guid.split_once("://")?;
        if scheme == "jellyfin" {
            let q = Q::new("/Items").s("Ids", id).s("Fields", PLAYABLE_FIELDS);
            return self.items_container(q, 0);
        }
        let flag = match scheme { "imdb" => "HasImdbId", "tmdb" => "HasTmdbId", "tvdb" => "HasTvdbId", _ => return None };
        // `AnyProviderIdEquals` is the provider-id filter; servers that ignore it answer the
        // provider-id-bearing set, so the result is filtered here too.
        let r = self.items(Q::new("/Items").b("Recursive", true).b(flag, true)
            .s("AnyProviderIdEquals", &format!("{}.{id}", provider_key(scheme)))
            .s("IncludeItemTypes", "Movie,Series,Episode").s("Fields", PLAYABLE_FIELDS).i("Limit", 10))?;
        let key = provider_key(scheme);
        let rows: Vec<Metadata> = r.items.iter()
            .filter(|i| i.provider_ids.get(key).is_some_and(|v| v == id))
            .map(|i| convert::item(i, 0)).collect();
        let n = rows.len() as i64;
        Some(convert::container(rows, n, 0))
    }

    // ---- user data ------------------------------------------------------------------------

    pub fn scrobble(&self, rk: &str) -> bool {
        let (Some(guid), Some(uid)) = (self.guid(rk), self.user_id()) else { return false };
        self.ok(&Q::new(format!("/UserPlayedItems/{guid}")).s("userId", &uid).build(), Method::Post, None)
    }

    pub fn unscrobble(&self, rk: &str) -> bool {
        let (Some(guid), Some(uid)) = (self.guid(rk), self.user_id()) else { return false };
        self.ok(&Q::new(format!("/UserPlayedItems/{guid}")).s("userId", &uid).build(), Method::Delete, None)
    }

    /// Jellyfin has no hide flag: an item leaves Resume when its position is cleared, so this
    /// resets the resume point (docs/jellyfin-port.md records the difference from PMS).
    pub fn remove_from_continue_watching(&self, rk: &str) -> bool {
        let (Some(guid), Some(uid)) = (self.guid(rk), self.user_id()) else { return false };
        self.post_ok(
            &Q::new(format!("/UserItems/{guid}/UserData")).s("userId", &uid).build(),
            Some(&serde_json::json!({ "PlaybackPositionTicks": 0 })),
        )
    }

    // ---- shelves ---------------------------------------------------------------------------

    /// Home's library shelves: one "Recently Added" per library (`/Items/Latest`, grouped), in
    /// the user's view order.
    pub fn home_shelves(&self, count: i64) -> Option<Vec<JfShelf>> {
        let uid = self.user_id()?;
        let views: QueryResult<BaseItemDto> = self.get(&Q::new("/UserViews").s("userId", &uid).build())?;
        // One `/Items/Latest` per library, asked together; the shelves keep the libraries' order.
        let latest = std::thread::scope(|s| {
            let asks: Vec<_> = views.items.iter().filter_map(|v| {
                let sec = convert::section(v)?;
                let parent = ids::normalize(&v.id);
                let uid = uid.as_str();
                Some((sec, nj_base::task::beside(s, "home-latest", move || -> Option<Vec<BaseItemDto>> {
                    self.get(&Q::new("/Items/Latest").s("userId", uid)
                        .s("ParentId", &parent).i("Limit", count).b("GroupItems", true)
                        .s("Fields", LIST_FIELDS).s("ImageTypeLimit", "1")
                        .s("EnableImageTypes", "Primary,Backdrop,Logo,Thumb").build())
                })))
            }).collect();
            asks.into_iter().map(|(sec, ask)| (sec, ask.finish())).collect::<Vec<_>>()
        });
        let mut out = Vec::new();
        for (sec, latest) in latest {
            let section: i64 = sec.key.parse().unwrap_or(0);
            let (identifier, type_num) = match sec.kind.as_str() {
                "show" => ("home.television.recent", 2),
                _ => ("home.movies.recent", 1),
            };
            let items = latest.unwrap_or_default();
            if !items.is_empty() {
                out.push(JfShelf {
                    identifier: identifier.into(),
                    title: format!("Recently Added in {}", sec.title),
                    key: format!("/hubs/home/recentlyAdded?type={type_num}&sectionID={section}"),
                    section,
                    library: sec.title,
                    items,
                });
            }
        }
        Some(out)
    }

    /// One library's shelves: Continue Watching (`/UserItems/Resume`), Next Up for TV
    /// (`/Shows/NextUp`) and Recently Added (`/Items/Latest`), empty ones dropped.
    pub fn library_shelves(&self, section_key: i64, count: i64) -> Option<Vec<JfShelf>> {
        let guid = ids::guid_of(section_key)?;
        let uid = self.user_id()?;
        let tv = self.view_kind(&guid).as_deref() == Some("tvshows");
        let family = if tv { "tv" } else { "movie" };
        let shelf = |identifier: String, title: &str, key: String, items: Vec<BaseItemDto>| JfShelf {
            identifier, title: title.into(), key, section: section_key, library: String::new(), items,
        };
        let mut out = Vec::new();
        if let Some(r) = self.items(Q::new("/UserItems/Resume").s("ParentId", &guid).i("Limit", count)
            .s("Fields", PLAYABLE_FIELDS).s("MediaTypes", "Video"))
        {
            out.push(shelf(format!("{family}.inprogress.{section_key}"), "Continue Watching",
                format!("/hubs/sections/{section_key}/continueWatching/items"), r.items));
        }
        if tv {
            if let Some(r) = self.items(Q::new("/Shows/NextUp").s("ParentId", &guid).i("Limit", count).s("Fields", PLAYABLE_FIELDS)) {
                out.push(shelf(format!("tv.ondeck.{section_key}"), "Next Up", String::new(), r.items));
            }
        }
        let latest: Option<Vec<BaseItemDto>> = self.get(&Q::new("/Items/Latest").s("userId", &uid)
            .s("ParentId", &guid).i("Limit", count).b("GroupItems", true).s("Fields", LIST_FIELDS).build());
        out.push(shelf(format!("{family}.recentlyadded.{section_key}"), "Recently Added", String::new(),
            latest.unwrap_or_default()));
        out.retain(|s| !s.items.is_empty());
        Some(out)
    }

    /// Continue Watching: in-progress items (newest first) followed by the Next Up episode of each
    /// series that has none in progress.
    pub fn continue_watching_items(&self, count: i64) -> Option<Vec<BaseItemDto>> {
        let resume = self.items(Q::new("/UserItems/Resume").i("Limit", count).s("Fields", PLAYABLE_FIELDS)
            .s("MediaTypes", "Video").s("ImageTypeLimit", "1").s("EnableImageTypes", "Primary,Backdrop,Logo,Thumb"))?;
        let next = self.items(Q::new("/Shows/NextUp").i("Limit", count).s("Fields", PLAYABLE_FIELDS)
            .b("EnableResumable", false).b("EnableRewatching", false)).unwrap_or_default();
        let in_progress_series: std::collections::HashSet<String> = resume.items.iter()
            .filter_map(|i| i.series_id.as_deref().map(ids::normalize)).collect();
        let mut rows = resume.items;
        for it in next.items {
            if rows.len() as i64 >= count {
                break;
            }
            if it.series_id.as_deref().map(ids::normalize).is_some_and(|s| in_progress_series.contains(&s)) {
                continue;
            }
            rows.push(it);
        }
        Some(rows)
    }

    pub fn promoted(&self) -> Option<MediaContainer> {
        Some(MediaContainer::default())
    }

    // ---- search ---------------------------------------------------------------------------

    /// Titles matching `query`, grouped Movies / Shows / Episodes / Collections, and the people
    /// whose name matches it.
    pub fn search_results(&self, query: &str, limit: i64) -> Option<JfSearch> {
        let limit = if limit > 0 { limit } else { 10 };
        let r = self.items(Q::new("/Items").s("searchTerm", query).b("Recursive", true)
            .s("IncludeItemTypes", "Movie,Series,Episode,BoxSet").i("Limit", limit * 4)
            .s("Fields", LIST_FIELDS))?;
        let mut groups: Vec<(&'static str, Vec<BaseItemDto>)> =
            SEARCH_GROUPS.iter().map(|(t, _, _)| (*t, Vec::new())).collect();
        for it in r.items {
            if let Some((_, rows)) = groups.iter_mut().find(|(t, rows)| *t == it.kind && (rows.len() as i64) < limit) {
                rows.push(it);
            }
        }
        let people: Option<QueryResult<BaseItemDto>> = self.get(&Q::new("/Persons").s("searchTerm", query)
            .i("Limit", limit).s("userId", &self.user_id().unwrap_or_default()).build());
        Some(JfSearch { groups, people: people.map(|p| p.items).unwrap_or_default() })
    }

    /// [`Self::search_results`] in the hub shape the person page's identity resolution reads.
    pub fn search(&self, query: &str, limit: i64, _section_id: i64) -> Option<MediaContainer> {
        let found = self.search_results(query, limit)?;
        let mut hubs: Vec<Hub> = found.groups.iter().zip(SEARCH_GROUPS).map(|((_, rows), (_, kind, title))| {
            convert::hub(kind, title, kind, "", rows.iter().map(|i| convert::item(i, 0)).collect())
        }).collect();
        let tags: Vec<Tag> = found.people.iter().map(person_hit).collect();
        hubs.push(Hub {
            kind: "actor".into(),
            hub_identifier: "actor".into(),
            title: "Cast & Crew".into(),
            size: tags.len() as i64,
            total_size: tags.len() as i64,
            directory: tags,
            ..Default::default()
        });
        Some(MediaContainer { size: hubs.len() as i64, hub: hubs, ..Default::default() })
    }

    // ---- collections ----------------------------------------------------------------------

    /// BoxSets are server-wide on Jellyfin, not per library; the section argument is kept so the
    /// rows can be stamped with the library that asked.
    pub(crate) fn section_collections(&self, section: i64, start: i64, size: i64) -> CollectionOutcome {
        let q = Q::new("/Items").s("IncludeItemTypes", "BoxSet").b("Recursive", true).s("SortBy", "SortName")
            .s("Fields", LIST_FIELDS).b("EnableTotalRecordCount", true).i("StartIndex", start).i("Limit", size);
        self.collection_page(q, section)
    }

    pub(crate) fn collection(&self, rk: &str) -> CollectionOutcome {
        let Some(guid) = self.guid(rk) else { return CollectionOutcome::Missing };
        let Some(uid) = self.user_id() else { return CollectionOutcome::Transport };
        let (st, it) = self.get_status::<BaseItemDto>(&Q::new(format!("/Items/{guid}")).s("userId", &uid).build());
        collection_outcome(st, it.map(|i| convert::container(vec![convert::item(&i, 0)], 1, 0)))
    }

    /// One page of a BoxSet's members.
    pub(crate) fn collection_members(&self, rk: &str, start: i64, size: i64) -> CollectionOutcome<QueryResult<BaseItemDto>> {
        let Some(guid) = self.guid(rk) else { return CollectionOutcome::Missing };
        let mut q = Q::new("/Items").s("ParentId", &guid).s("Fields", LIST_FIELDS)
            .s("SortBy", "PremiereDate,SortName").b("EnableTotalRecordCount", true).i("StartIndex", start);
        if size > 0 {
            q = q.i("Limit", size);
        }
        self.collection_query(q)
    }

    fn collection_query(&self, q: Q) -> CollectionOutcome<QueryResult<BaseItemDto>> {
        let q = match self.user_id() {
            Some(uid) => q.s("userId", &uid),
            None => return CollectionOutcome::Transport,
        };
        let (st, r) = self.get_status::<QueryResult<BaseItemDto>>(&q.build());
        collection_outcome(st, r)
    }

    fn collection_page(&self, q: Q, section: i64) -> CollectionOutcome {
        match self.collection_query(q) {
            CollectionOutcome::Ok(r) => {
                let rows = r.items.iter().map(|i| convert::item(i, section)).collect();
                CollectionOutcome::Ok(convert::container(rows, r.total_record_count, r.start_index))
            }
            CollectionOutcome::Denied => CollectionOutcome::Denied,
            CollectionOutcome::Missing => CollectionOutcome::Missing,
            CollectionOutcome::Transport => CollectionOutcome::Transport,
        }
    }

    // ---- subtitles ------------------------------------------------------------------------

    /// An external subtitle stream (the key [`convert::stream`] built) as UTF-8 text; ASS/SSA keep
    /// their own format, everything else is asked for as SubRip.
    pub fn sidecar_subtitle(&self, key: &str, codec: &str) -> Option<Vec<u8>> {
        if !key.starts_with("/Videos/") || !key.contains("/Subtitles/") {
            return None;
        }
        let path = if codec.eq_ignore_ascii_case("ass") || codec.eq_ignore_ascii_case("ssa") {
            key.to_string()
        } else {
            match key.rsplit_once("/Stream.") {
                Some((head, _)) => format!("{head}/Stream.srt"),
                None => key.to_string(),
            }
        };
        let r = self.send(&path, Method::Get, None)?;
        (r.ok() && !r.body.is_empty() && r.body.len() <= crate::catalog::SIDECAR_MAX_BYTES).then_some(r.body)
    }
}

/// A person search hit as the search shelf's tag row.
fn person_hit(p: &BaseItemDto) -> Tag {
    Tag {
        tag: p.name.clone(),
        id: ids::intern(&p.id),
        tag_key: ids::normalize(&p.id),
        thumb: super::images::person_thumb(p),
        ..Default::default()
    }
}

fn collection_outcome<T>(status: Option<i32>, page: Option<T>) -> CollectionOutcome<T> {
    match (status, page) {
        (Some(401 | 403), _) => CollectionOutcome::Denied,
        (Some(404), _) => CollectionOutcome::Missing,
        (Some(200..=299), Some(p)) => CollectionOutcome::Ok(p),
        _ => CollectionOutcome::Transport,
    }
}

fn provider_key(scheme: &str) -> &'static str {
    match scheme {
        "imdb" => "Imdb",
        "tmdb" => "Tmdb",
        _ => "Tvdb",
    }
}

fn parse<T: DeserializeOwned>(path: &str, body: &[u8]) -> Option<T> {
    match serde_json::from_slice::<T>(body) {
        Ok(v) => Some(v),
        Err(e) => {
            nj_base::eventlog::log(&format!(
                "jf: GET {} answered {} bytes that will not parse — {e}",
                path.split('?').next().unwrap_or(path),
                body.len()
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plex_sort_expressions_translate_to_items_sorts() {
        assert_eq!(sort_by("titleSort:asc"), ("SortName".to_string(), "Ascending"));
        assert_eq!(sort_by("addedAt:desc"), ("DateCreated,SortName".to_string(), "Descending"));
        assert_eq!(sort_by("showOrder:desc").0, "SeriesSortName,ParentIndexNumber,IndexNumber,SortName");
        assert_eq!(sort_by("random").0, "Random");
        assert_eq!(sort_by("").0, "SortName");
        assert_eq!(sort_by("nonsense:desc"), ("SortName".to_string(), "Descending"));
    }

    #[test]
    fn the_query_builder_encodes_values_and_skips_empty_ones() {
        assert_eq!(Q::new("/Items").s("searchTerm", "tom & jerry").s("ParentId", "").i("Limit", 5).build(),
            "/Items?searchTerm=tom%20%26%20jerry&Limit=5");
        assert_eq!(Q::new("/x?a=1").b("Recursive", true).build(), "/x?a=1&Recursive=true");
    }

    #[test]
    fn every_advertised_sort_is_one_the_translator_knows() {
        for t in ["Movie", "Series", "Episode", "Season"] {
            for s in &sort_menu(t).types[0].sort {
                let (fields, _) = sort_by(&format!("{}:asc", s.key));
                assert!(fields != "SortName" || s.key == "titleSort", "{} unmapped", s.key);
            }
        }
    }
}
