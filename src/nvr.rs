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
use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, Offset, TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

pub struct Client {
    pub nvr: StoredNvr,
    pub tz: FixedOffset,
    pub channel_by_host: HashMap<String, u32>,
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
