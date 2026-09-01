// update.rs — self-update against GitHub Releases (port of Updater.swift).
//
// The check compares the baked-in CARGO_PKG_VERSION against the latest
// release tag (semver core only). Applying runs the official curl|bash
// installer — the same script that did the original install owns every byte
// on disk (binary, bundled ffmpeg, licenses) — then the app relaunches onto
// the new files. Only a binary running from the install dir self-updates;
// a source build gets told to pull and rebuild instead.

use std::sync::mpsc::Sender;
use std::time::Duration;

/// Must match REPO in install.sh and the release workflow.
pub const REPO: &str = "alkait/HikYeah";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// releases/latest always resolves to the newest tagged release, so this
/// URL never goes stale.
const INSTALLER_URL: &str = "https://github.com/alkait/HikYeah/releases/latest/download/install.sh";

#[derive(Clone)]
pub struct Release {
    pub tag: String,
    pub notes_url: String,
}

pub enum Msg {
    Available(Release),
    UpToDate(String),
    CheckFailed(String),
    /// Installer succeeded — the UI relaunches onto the new files.
    Installed,
    InstallFailed(String),
}

/// Whether this binary runs from the installer's directory. A source build
/// (target/release, anywhere else) must not have the installer write over it.
pub fn installed() -> bool {
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .map(|b| b.join("hikyeah"));
    match (std::env::current_exe(), dir) {
        (Ok(exe), Some(dir)) => exe.starts_with(&dir),
        _ => false,
    }
}

/// Background check; the outcome arrives on `tx` (UI thread repainted).
pub fn check(tx: Sender<Msg>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let msg = match fetch_latest() {
            Err(e) => Msg::CheckFailed(e),
            Ok(rel) if semver_less(VERSION, &rel.tag) => Msg::Available(rel),
            Ok(rel) => Msg::UpToDate(rel.tag),
        };
        if tx.send(msg).is_ok() {
            ctx.request_repaint();
        }
    });
}

fn fetch_latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(6))
        .build();
    let resp = agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("GitHub returned HTTP {code}"),
            e => e.to_string(),
        })?;
    let body = resp.into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let tag = v.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
    if tag.is_empty() {
        return Err("could not parse the latest release from GitHub".into());
    }
    Ok(Release {
        tag: tag.to_string(),
        notes_url: v
            .get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Run the installer; it swaps the install dir under us (our process and its
/// ffmpeg children keep their old inodes until the relaunch).
pub fn apply(tx: Sender<Msg>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("/bin/bash -c \"$(curl -fsSL {INSTALLER_URL})\""))
            .output();
        let msg = match out {
            Ok(o) if o.status.success() => Msg::Installed,
            Ok(o) => {
                // The installer's die() writes the reason to stderr last.
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                let all = if err.trim().is_empty() {
                    String::from_utf8_lossy(&o.stdout).to_string()
                } else {
                    err
                };
                let last = all
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("installer failed")
                    .to_string();
                Msg::InstallFailed(last)
            }
            Err(e) => Msg::InstallFailed(format!("could not run the installer: {e}")),
        };
        if tx.send(msg).is_ok() {
            ctx.request_repaint();
        }
    });
}

pub fn open_url(url: &str) {
    if url.is_empty() {
        return;
    }
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    let _ = cmd.spawn();
}

/// Whether `a` is strictly older than `b`, comparing only the
/// MAJOR.MINOR.PATCH core: a leading "v" and -pre/+build suffixes are
/// ignored. Unparseable input compares as not-less (stay quiet).
fn semver_less(a: &str, b: &str) -> bool {
    match (semver_core(a), semver_core(b)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

fn semver_core(v: &str) -> Option<(u64, u64, u64)> {
    let s = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let s = &s[..s.find(['-', '+']).unwrap_or(s.len())];
    let mut it = s.split('.');
    let (a, b, c) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    Some((a.parse().ok()?, b.parse().ok()?, c.parse().ok()?))
}
