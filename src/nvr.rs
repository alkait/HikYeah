// nvr.rs — read-only ISAPI client for the NVR's recordings (NVRClient.swift
// port). Three things: channel discovery (camera IP -> NVR channel, so
// playback needs zero per-camera setup), the NVR's UTC offset, and
// recorded-segment search. Timestamps over ISAPI carry a 'Z' suffix but are
// actually the NVR's *local* time (long-standing Hikvision quirk — a real
// UTC "future" time gets a 400), so every format/parse here uses the NVR's
// own offset, never this machine's. Every call blocks; callers run them on
// a thread and hand the result back over a channel.

use crate::config::{StoredNvr, url_encode};
use crate::isapi::{self, blocks, tag};
use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, Offset, TimeDelta, TimeZone, Utc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

pub struct Client {
    pub nvr: StoredNvr,
    pub tz: FixedOffset,
    pub channel_by_host: HashMap<String, u32>,
    events: Mutex<EventState>,
}

/// Spans per NVR channel.
pub type Spans = HashMap<u32, Vec<Segment>>;

/// Motion and intrusion spans for every channel of one window, from the
/// NVR's alarm log (motionStart/Stop and fieldDetectionStart/Stop pairs —
/// the recordings themselves are continuous and carry no event typing).
#[derive(Default)]
pub struct EventLog {
    pub motion: Spans,
    pub intrusion: Spans,
}

/// Caches and the crawl queue. The NVR's log search runs ONE server-side
/// session: each crawl re-sends its searchID to page that session, so two
/// concurrent crawls (e.g. the launch warm-up covering today + yesterday)
/// interleave searchIDs, keep resetting each other's session, and can wedge
/// into pagination that never terminates — callers then hang on "loading"
/// forever. Serialize: one crawl at a time, the rest wait their turn.
#[derive(Default)]
struct EventState {
    /// By window start (unix seconds): when fetched, and the log.
    log_cache: HashMap<i64, (Instant, Arc<EventLog>)>,
    /// Deliveries waiting on an in-flight crawl, by window start.
    log_pending: HashMap<i64, Vec<Sender<Arc<EventLog>>>>,
    queue: VecDeque<(DateTime<Utc>, DateTime<Utc>)>,
    crawling: bool,
    /// "channel|type|from": AcuSense-classified spans.
    target_cache: HashMap<String, (Instant, Arc<Vec<Segment>>)>,
}

/// Past days never change; today's entries go stale as new events land.
fn cache_valid(stamp: Instant, window_end: DateTime<Utc>) -> bool {
    window_end < Utc::now() || stamp.elapsed() < Duration::from_secs(60)
}

/// Recording track for an NVR channel: main-stream recording is
/// channel*100 + 1 (channel 7 -> track 701).
pub fn track(channel: u32) -> u32 {
    channel * 100 + 1
}

impl Client {
    /// Fetch the timezone + channel map (NVRClient.prepare). A missing
    /// timezone keeps this machine's; an empty channel map is a failure.
    pub fn prepare(nvr: StoredNvr) -> Result<Client, String> {
        let tz = get(&nvr, "/ISAPI/System/time")
            .and_then(|b| parse_time_zone(&String::from_utf8_lossy(&b)))
            .unwrap_or_else(|| Local::now().offset().fix());
        let body = get(&nvr, "/ISAPI/ContentMgmt/InputProxy/channels").ok_or("NVR unreachable")?;
        let channel_by_host = parse_channels(&String::from_utf8_lossy(&body));
        if channel_by_host.is_empty() {
            return Err("NVR unreachable".into());
        }
        Ok(Client {
            nvr,
            tz,
            channel_by_host,
            events: Mutex::default(),
        })
    }

    /// All recorded segments for `track` in [from, to), merged and sorted.
    /// Pages through the search API (the NVR caps each response).
    pub fn search_segments(
        &self,
        track: u32,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Vec<Segment> {
        let search_id = search_id();
        let mut all = Vec::new();
        let mut position = 0;
        loop {
            // "searchResultPostion" is the NVR's own spelling.
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                 <CMSearchDescription>\
                 <searchID>{search_id}</searchID>\
                 <trackList><trackID>{track}</trackID></trackList>\
                 <timeSpanList><timeSpan><startTime>{}</startTime><endTime>{}</endTime></timeSpan></timeSpanList>\
                 <maxResults>64</maxResults>\
                 <searchResultPostion>{position}</searchResultPostion>\
                 <metadataList><metadataDescriptor>//recordType.meta.std-cgi.com</metadataDescriptor></metadataList>\
                 </CMSearchDescription>",
                self.fmt_z(from),
                self.fmt_z(to)
            );
            let Some(resp) = self.post("/ISAPI/ContentMgmt/search", &body, 10) else {
                break;
            };
            let xml = String::from_utf8_lossy(&resp);
            let mut segs = Vec::new();
            for item in blocks(&xml, "searchMatchItem") {
                if let (Some(s), Some(e)) = (
                    tag(item, "startTime").and_then(|t| self.parse_z(t)),
                    tag(item, "endTime").and_then(|t| self.parse_z(t)),
                ) && e > s
                {
                    segs.push(Segment { start: s, end: e });
                }
            }
            let more = tag(&xml, "responseStatusStrg").map(str::trim) == Some("MORE");
            let n = segs.len();
            all.extend(segs);
            if more && n > 0 {
                position += n;
            } else {
                break;
            }
        }
        merge(all)
    }

    /// Which days of a month have any recording on `track` (drives the
    /// calendar's enabled days).
    pub fn recorded_days(&self, track: u32, year: i32, month: u32) -> HashSet<u32> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <trackDailyParam><year>{year}</year><monthOfYear>{month}</monthOfYear></trackDailyParam>"
        );
        let path = format!("/ISAPI/ContentMgmt/record/tracks/{track}/dailyDistribution");
        let mut days = HashSet::new();
        if let Some(resp) = self.post(&path, &body, 8) {
            let xml = String::from_utf8_lossy(&resp);
            for day in blocks(&xml, "day") {
                if tag(day, "record").map(str::trim) == Some("true")
                    && let Some(d) = tag(day, "dayOfMonth").and_then(|d| d.trim().parse().ok())
                {
                    days.insert(d);
                }
            }
        }
        days
    }

    /// RTSP path + clock string replaying `track` for [from, to). The NVR
    /// stops sending at `to`; the stalled read is the "segment ended" signal.
    pub fn playback_request(
        &self,
        track: u32,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> (String, String) {
        let f = |t: DateTime<Utc>| {
            t.with_timezone(&self.tz)
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        };
        let s = f(from);
        (
            format!("/Streaming/tracks/{track}/?starttime={s}&endtime={}", f(to)),
            s,
        )
    }

    /// Credentialed RTSP URL for `path` (MediaSaver.playbackURL) — ffmpeg's
    /// own RTSP client, for 1× clips and playback snapshots.
    pub fn rtsp_url(&self, path: &str) -> String {
        format!(
            "rtsp://{}:{}@{}:{}{path}",
            url_encode(&self.nvr.user),
            url_encode(&self.nvr.password),
            self.nvr.host,
            self.nvr.rtsp_port()
        )
    }

    // MARK: time, in the NVR's zone

    pub fn local(&self, t: DateTime<Utc>) -> DateTime<FixedOffset> {
        t.with_timezone(&self.tz)
    }

    /// Midnight of `t`'s day in the NVR's zone. Fixed offsets have no DST,
    /// so a day is exactly 86 400 s: `day + 1 day` is plain addition.
    pub fn start_of_day(&self, t: DateTime<Utc>) -> DateTime<Utc> {
        let local = self.local(t);
        self.tz
            .from_local_datetime(&local.date_naive().and_hms_opt(0, 0, 0).unwrap())
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Request/response timestamps: NVR-local digits with a fake 'Z'.
    fn fmt_z(&self, t: DateTime<Utc>) -> String {
        self.local(t).format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn parse_z(&self, s: &str) -> Option<DateTime<Utc>> {
        let naive = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%dT%H:%M:%SZ").ok()?;
        Some(
            self.tz
                .from_local_datetime(&naive)
                .single()?
                .with_timezone(&Utc),
        )
    }

    fn post(&self, path: &str, body: &str, timeout_secs: u64) -> Option<Vec<u8>> {
        isapi::request(
            &self.nvr.host,
            &self.nvr.user,
            &self.nvr.password,
            path,
            Some(("application/xml", body.as_bytes())),
            Duration::from_secs(timeout_secs),
        )
    }
}

impl Client {
    // MARK: event log (generic motion + intrusion) + human/vehicle classified

    /// Motion and intrusion spans for ALL channels in [from, to). One fetch
    /// serves every camera and both event types. May deliver twice on `tx`:
    /// instantly with cached data (even stale), then again with fresh data
    /// once a revalidating crawl lands; the sender is dropped when nothing
    /// more will come. Concurrent calls for the same window share one crawl.
    pub fn event_log(
        self: &Arc<Self>,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        tx: Sender<Arc<EventLog>>,
    ) {
        let key = from.timestamp();
        let mut st = self.events.lock().unwrap();
        if let Some((stamp, log)) = st.log_cache.get(&key) {
            let _ = tx.send(log.clone());
            if cache_valid(*stamp, to) {
                return;
            }
            // Stale (today, >60 s old) — deliver again after revalidating.
        }
        if let Some(waiting) = st.log_pending.get_mut(&key) {
            waiting.push(tx); // a crawl is already running or queued
            return;
        }
        st.log_pending.insert(key, vec![tx]);
        st.queue.push_back((from, to));
        if st.crawling {
            return;
        }
        st.crawling = true;
        drop(st);
        let me = self.clone();
        std::thread::spawn(move || {
            loop {
                let next = me.events.lock().unwrap().queue.pop_front();
                let Some((from, to)) = next else {
                    me.events.lock().unwrap().crawling = false;
                    return;
                };
                let log = Arc::new(me.crawl_log(from, to));
                let mut st = me.events.lock().unwrap();
                st.log_cache
                    .insert(from.timestamp(), (Instant::now(), log.clone()));
                for tx in st.log_pending.remove(&from.timestamp()).unwrap_or_default() {
                    let _ = tx.send(log.clone());
                }
            }
        });
    }

    /// The stitched alarm-log crawl. The NVR silently truncates every log
    /// search at 2000 entries — no error, no MORE flag, it just looks like a
    /// clean end of data. All log types count against the cap (the subName
    /// filter below is ignored), so on a busy day a full-day search ends
    /// hours early and the rest of the day shows no motion. When a search
    /// dies at the cap, stitch: run a fresh search from the last entry's
    /// timestamp (inclusive, so same-second entries survive; `seen` dedupes
    /// the overlap) until the window is genuinely covered.
    fn crawl_log(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> EventLog {
        const SEARCH_CAP: usize = 2000;
        let started = Instant::now();
        let mut motion: Vec<(u32, bool, DateTime<Utc>)> = Vec::new();
        let mut intrusion: Vec<(u32, bool, DateTime<Utc>)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new(); // "metaId|time" across stitched searches
        let mut cursor = from;
        let mut restarts_left = 16;
        'sessions: loop {
            // One searchID for the whole session: re-sending it lets the NVR
            // serve pages from its existing server-side search (~10 ms each);
            // a fresh ID per page makes it re-run the search every time
            // (~570 ms each — 31 s for a busy day).
            let search_id = search_id();
            let mut session_count = 0usize;
            let mut last_time = cursor;
            let mut position = 0usize;
            loop {
                // Termination backstop: no sane day has this many log
                // entries; beyond it assume the NVR is stuck answering MORE
                // and bail with what we have rather than paging forever.
                if position >= 50_000 {
                    break 'sessions;
                }
                let body = format!(
                    "<CMSearchDescription><searchID>{search_id}</searchID><metaId>log.std-cgi.com</metaId>\
                     <timeSpanList><timeSpan><startTime>{}</startTime><endTime>{}</endTime></timeSpan></timeSpanList>\
                     <maxResults>64</maxResults><searchResultPostion>{position}</searchResultPostion>\
                     <metadataList><metadataDescriptor>//metadata.std-cgi.com/types/logs?name=alarm&amp;subName=motionalarm</metadataDescriptor></metadataList>\
                     </CMSearchDescription>",
                    self.fmt_z(cursor),
                    self.fmt_z(to)
                );
                let Some(resp) = self.post("/ISAPI/ContentMgmt/logSearch", &body, 10) else {
                    break 'sessions;
                };
                let xml = String::from_utf8_lossy(&resp);
                let items: Vec<(&str, &str)> = blocks(&xml, "searchMatchItem")
                    .into_iter()
                    .filter_map(|item| {
                        let meta = tag(item, "metaId")?.trim();
                        let time = tag(item, "StartDateTime")?.trim();
                        (!meta.is_empty() && !time.is_empty()).then_some((meta, time))
                    })
                    .collect();
                session_count += items.len();
                for (meta, time) in &items {
                    // Response times: local, no suffix.
                    let Some(t) = NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H:%M:%S")
                        .ok()
                        .and_then(|n| self.tz.from_local_datetime(&n).single())
                        .map(|d| d.with_timezone(&Utc))
                    else {
                        continue;
                    };
                    if t > last_time {
                        last_time = t;
                    }
                    if !seen.insert(format!("{meta}|{time}")) {
                        continue;
                    }
                    // metaId: log.hikvision.com/Alarm/motionStart/15,
                    //         …/Alarm/fieldDetectionStart/9 (= intrusion)
                    let parts: Vec<&str> = meta.split('/').collect();
                    let (Some(kind), Some(ch)) = (
                        parts.len().checked_sub(2).map(|i| parts[i]),
                        parts.last().and_then(|c| c.parse::<u32>().ok()),
                    ) else {
                        continue;
                    };
                    match kind {
                        "motionStart" => motion.push((ch, true, t)),
                        "motionStop" => motion.push((ch, false, t)),
                        "fieldDetectionStart" => intrusion.push((ch, true, t)),
                        "fieldDetectionStop" => intrusion.push((ch, false, t)),
                        _ => {}
                    }
                }
                let more = tag(&xml, "responseStatusStrg").map(str::trim) == Some("MORE");
                if more && !items.is_empty() {
                    position += items.len();
                } else if session_count >= SEARCH_CAP
                    && last_time > cursor
                    && last_time < to
                    && restarts_left > 0
                {
                    // Quirk: the NVR filters *Stop entries by their event's
                    // START time, so restarting exactly at the cap would drop
                    // the stop of any motion still running across the restart
                    // point (leaving a falsely open span). Back the cursor
                    // off 30 min to re-cover straddlers; `seen` dedupes the
                    // overlap, and max() keeps forward progress.
                    cursor =
                        (cursor + TimeDelta::seconds(1)).max(last_time - TimeDelta::seconds(1800));
                    restarts_left -= 1;
                    continue 'sessions;
                } else {
                    break 'sessions;
                }
            }
        }
        let log = EventLog {
            motion: pair_events(&motion, from, to),
            intrusion: pair_events(&intrusion, from, to),
        };
        if std::env::var_os("HIK_DEBUG").is_some() {
            eprintln!(
                "[nvr] alarm log {}: {} entries, {} motion / {} intrusion spans, {:.1} s, {} restarts",
                self.local(from).format("%F"),
                seen.len(),
                log.motion.values().map(Vec::len).sum::<usize>(),
                log.intrusion.values().map(Vec::len).sum::<usize>(),
                started.elapsed().as_secs_f32(),
                16 - restarts_left
            );
        }
        log
    }

    /// AcuSense-classified motion spans ("human" or "vehicle") for one
    /// channel in [from, to) via /ISAPI/ContentMgmt/SearchByTargetType — the
    /// same API behind the NVR web player's Human/Vehicle checkboxes.
    /// Response times look like real ISO 8601 with offsets, but the offset
    /// lies on fresh records: for ~20–80 s after an event the NVR reports the
    /// local wall clock with "+00:00", then re-reports the same record with
    /// the true offset. Trusting it threw just-happened events 4 h into the
    /// future — a phantom tick beyond the recorded band — so parse the digits
    /// as NVR-local and ignore the offset.
    pub fn classified_spans(
        &self,
        channel: u32,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        target: &str,
    ) -> Arc<Vec<Segment>> {
        let key = format!("{channel}|{target}|{}", from.timestamp());
        if let Some((stamp, spans)) = self.events.lock().unwrap().target_cache.get(&key)
            && cache_valid(*stamp, to)
        {
            return spans.clone();
        }
        let fmt = |t: DateTime<Utc>| self.local(t).format("%Y-%m-%dT%H:%M:%S%:z").to_string();
        let parse = |v: &serde_json::Value| -> Option<DateTime<Utc>> {
            let s = v.as_str()?;
            if s.len() < 19 {
                return None;
            }
            let n = NaiveDateTime::parse_from_str(&s[..19], "%Y-%m-%dT%H:%M:%S").ok()?;
            Some(
                self.tz
                    .from_local_datetime(&n)
                    .single()?
                    .with_timezone(&Utc),
            )
        };
        let search_id = search_id(); // one per search: pages reuse the NVR's session
        let mut all = Vec::new();
        let mut position = 0usize;
        loop {
            let body = serde_json::json!({
                "SearchDescription": {
                    "searchID": search_id,
                    "searchResultPosition": position,
                    "maxResults": 100,
                    "SearchCondList": [{
                        "channelID": channel,
                        "targetTypes": [target],
                        "searchTimeList": [{"searchTime": {"startTime": fmt(from), "endTime": fmt(to)}}],
                    }],
                }
            })
            .to_string();
            let Some(resp) = isapi::request(
                &self.nvr.host,
                &self.nvr.user,
                &self.nvr.password,
                "/ISAPI/ContentMgmt/SearchByTargetType?format=json",
                Some(("application/json", body.as_bytes())),
                Duration::from_secs(10),
            ) else {
                break;
            };
            let Ok(root) = serde_json::from_slice::<serde_json::Value>(&resp) else {
                break;
            };
            let result = &root["SearchResult"];
            let status = result["responseStatusStrg"].as_str().unwrap_or("");
            let mut count = 0usize;
            for m in result["matchList"].as_array().into_iter().flatten() {
                for info in m["RecordInfoList"].as_array().into_iter().flatten() {
                    count += 1;
                    if let (Some(s), Some(e)) = (
                        parse(&info["RecordTime"]["startTime"]),
                        parse(&info["RecordTime"]["endTime"]),
                    ) && e > s
                    {
                        all.push(Segment { start: s, end: e });
                    }
                }
            }
            if status == "MORE" && count > 0 {
                position += count;
            } else {
                break;
            }
        }
        let spans = Arc::new(merge(all));
        self.events
            .lock()
            .unwrap()
            .target_cache
            .insert(key, (Instant::now(), spans.clone()));
        spans
    }
}

/// Start/stop entries into spans. A channel can have overlapping events
/// (the NVR logs each detection region independently: start/start/stop/
/// stop), so track open depth — the span closes only when every open event
/// has stopped. Otherwise the second stop looks orphaned and takes the
/// began-before-window fallback, painting a false band from the window
/// start.
fn pair_events(
    events: &[(u32, bool, DateTime<Utc>)],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Spans {
    let mut sorted: Vec<&(u32, bool, DateTime<Utc>)> = events.iter().collect();
    sorted.sort_by_key(|e| e.2);
    let mut out: Spans = HashMap::new();
    let mut open: HashMap<u32, (DateTime<Utc>, u32)> = HashMap::new();
    for &(ch, is_start, t) in sorted {
        if is_start {
            match open.get_mut(&ch) {
                Some(o) => o.1 += 1,
                None => {
                    open.insert(ch, (t, 1));
                }
            }
        } else if let Some(o) = open.get_mut(&ch) {
            if o.1 > 1 {
                o.1 -= 1;
            } else {
                let since = o.0;
                open.remove(&ch);
                out.entry(ch).or_default().push(Segment {
                    start: since,
                    end: t,
                });
            }
        } else {
            // Stop without any start: motion began before the window.
            out.entry(ch).or_default().push(Segment {
                start: from,
                end: t,
            });
        }
    }
    let clamp = to.min(Utc::now());
    for (ch, (since, _)) in open {
        if since < clamp {
            out.entry(ch).or_default().push(Segment {
                start: since,
                end: clamp,
            });
        }
    }
    out.into_iter().map(|(ch, v)| (ch, merge(v))).collect()
}

fn get(nvr: &StoredNvr, path: &str) -> Option<Vec<u8>> {
    isapi::request(
        &nvr.host,
        &nvr.user,
        &nvr.password,
        path,
        None,
        Duration::from_secs(8),
    )
}

/// One id per search: re-sending it lets the NVR page its server-side
/// session instead of re-running the search. Any unique string will do —
/// UUID-shaped from the clock and a counter.
fn search_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
        (nanos >> 32) as u32,
        (nanos >> 16) as u16,
        nanos as u16,
        n as u16,
        std::process::id() as u64 ^ (n << 20)
    )
}

/// Offset from <localTime>2026-07-19T08:32:13+04:00</localTime>.
fn parse_time_zone(xml: &str) -> Option<FixedOffset> {
    let t = tag(xml, "localTime")?.trim();
    if t.len() < 6 {
        return None;
    }
    let off = &t[t.len() - 6..]; // "+04:00"
    let sign = if off.starts_with('-') { -1 } else { 1 };
    let (h, m) = off[1..].split_once(':')?;
    let secs = sign * (h.parse::<i32>().ok()? * 3600 + m.parse::<i32>().ok()? * 60);
    FixedOffset::east_opt(secs)
}

/// <InputProxyChannel><id>N</id>…<ipAddress>x.x.x.x</ipAddress>… -> {ip: N}.
/// The first <id> in a channel block is the channel's own (the descriptor
/// nested inside carries more).
fn parse_channels(xml: &str) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    for ch in blocks(xml, "InputProxyChannel") {
        let id = tag(ch, "id").and_then(|s| s.trim().parse::<u32>().ok());
        let ip = blocks(ch, "ipAddress")
            .into_iter()
            .map(str::trim)
            .find(|s| !s.is_empty());
        if let (Some(id), Some(ip)) = (id, ip) {
            map.insert(ip.to_string(), id);
        }
    }
    map
}

/// Sort and join segments closer than 2 s.
pub fn merge(mut raw: Vec<Segment>) -> Vec<Segment> {
    raw.sort_by_key(|s| s.start);
    let mut out: Vec<Segment> = Vec::new();
    for s in raw {
        match out.last_mut() {
            Some(last) if (s.start - last.end).num_seconds() < 2 => {
                if s.end > last.end {
                    last.end = s.end;
                }
            }
            _ => out.push(s),
        }
    }
    out
}
