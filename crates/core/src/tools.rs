//! Programs the app installs by itself for YouTube, so nobody has to set up Python: the
//! standalone yt-dlp.exe (it carries the scripts for YouTube's JavaScript challenges) and
//! Deno, the JavaScript runtime they run on. Both come from their official GitHub
//! releases and are checked against the checksums those releases publish.

use std::{
    fs,
    io::{Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

use sha2::{Digest, Sha256};

use crate::config::tools_dir;

const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const YTDLP_SUMS: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS";
const DENO_URL: &str = "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip";
/// yt-dlp is updated at most this often: YouTube changes, and old copies stop working.
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 3600);
/// Keeps a console window from flashing up.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn ytdlp_exe() -> PathBuf {
    tools_dir().join("yt-dlp.exe")
}

pub fn deno_exe() -> PathBuf {
    tools_dir().join("deno.exe")
}

/// Both programs are in place.
pub fn installed() -> bool {
    ytdlp_exe().exists() && deno_exe().exists()
}

/// What `install` is doing, for the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    YtDlp,
    Deno,
}

/// Downloads whatever is missing. Safe to call again after a failure: finished parts stay.
pub fn install(progress: impl Fn(Step)) -> Result<(), String> {
    let dir = tools_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("tools: {e}"))?;
    let agent = crate::http::agent(Duration::from_secs(900));
    if !ytdlp_exe().exists() {
        progress(Step::YtDlp);
        let sums = text(&agent, YTDLP_SUMS)?;
        let hash = sum_for(&sums, "yt-dlp.exe").ok_or("tools: yt-dlp checksum missing")?;
        download_checked(&agent, YTDLP_URL, &hash, &ytdlp_exe())?;
    }
    if !deno_exe().exists() {
        progress(Step::Deno);
        let hash = first_hash(&text(&agent, &format!("{DENO_URL}.sha256sum"))?).ok_or("tools: Deno checksum missing")?;
        let zip = dir.join("deno.zip");
        download_checked(&agent, DENO_URL, &hash, &zip)?;
        let result = unzip(&zip, &dir);
        let _ = fs::remove_file(&zip);
        result?;
        if !deno_exe().exists() {
            return Err("tools: deno.exe not in the archive".into());
        }
    }
    Ok(())
}

/// Brings the app's own yt-dlp up to date, at most once a day. Best effort: an old copy
/// still works until YouTube changes again.
pub fn update_ytdlp() {
    let marker = tools_dir().join("updated-at");
    let recent = fs::metadata(&marker).and_then(|m| m.modified()).ok().and_then(|at| SystemTime::now().duration_since(at).ok());
    if !ytdlp_exe().exists() || recent.is_some_and(|age| age < UPDATE_EVERY) {
        return;
    }
    match Command::new(ytdlp_exe()).arg("-U").creation_flags(CREATE_NO_WINDOW).output() {
        Ok(out) if out.status.success() => {
            let _ = fs::write(&marker, b"");
        }
        Ok(out) => eprintln!("yt-dlp not updated: {}", String::from_utf8_lossy(&out.stderr).trim()),
        Err(e) => eprintln!("yt-dlp not updated: {e}"),
    }
}

fn text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let mut resp = agent.get(url).call().map_err(|e| format!("network: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("tools: {url} returned {}", resp.status().as_u16()));
    }
    resp.body_mut().read_to_string().map_err(|e| format!("network: {e}"))
}

/// Streams `url` to `target`, which only appears once the checksum matches.
fn download_checked(agent: &ureq::Agent, url: &str, sha256: &str, target: &Path) -> Result<(), String> {
    let resp = agent.get(url).call().map_err(|e| format!("network: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("tools: {url} returned {}", resp.status().as_u16()));
    }
    let partial = target.with_extension("part");
    let result = (|| -> Result<String, String> {
        let mut reader = resp.into_body().into_reader();
        let mut file = fs::File::create(&partial).map_err(|e| format!("tools: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buffer).map_err(|e| format!("network: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            file.write_all(&buffer[..n]).map_err(|e| format!("tools: {e}"))?;
        }
        Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })();
    match result {
        Ok(got) if got.eq_ignore_ascii_case(sha256) => fs::rename(&partial, target).map_err(|e| format!("tools: {e}")),
        Ok(_) => {
            let _ = fs::remove_file(&partial);
            Err(format!("tools: checksum mismatch for {url}"))
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Windows' own tar reads zip files.
fn unzip(zip: &Path, into: &Path) -> Result<(), String> {
    let tar = std::env::var("SystemRoot").map_or_else(|_| PathBuf::from("tar"), |root| Path::new(&root).join("System32").join("tar.exe"));
    let out = Command::new(tar).arg("-xf").arg(zip).arg("-C").arg(into).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| format!("tools: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("tools: unzip failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The hash of `file` in a `sha256sum`-style list ("<hash>  <name>").
fn sum_for(list: &str, file: &str) -> Option<String> {
    list.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim() == file && is_sha256(hash)).then(|| hash.to_lowercase())
    })
}

/// The first SHA-256 in a text, whatever its layout (Deno publishes PowerShell's format).
fn first_hash(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit()).find(|word| is_sha256(word)).map(str::to_lowercase)
}

fn is_sha256(word: &str) -> bool {
    word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_checksum_layouts() {
        let sums = "aaaa  yt-dlp_x86.exe\nf5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d  yt-dlp.exe\n";
        assert_eq!(sum_for(sums, "yt-dlp.exe").as_deref(), Some("f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d"));
        assert_eq!(sum_for(sums, "yt-dlp_x86.exe"), None);
        let deno = "\r\nAlgorithm : SHA256\r\nHash      : CBA84B26E0DF5AE7F74FC66B86A95B866872A86292EBD84FA1E959927EDD93BF\r\nPath      : C:\\a\\deno.zip\r\n";
        assert_eq!(first_hash(deno).as_deref(), Some("cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf"));
        assert_eq!(first_hash("no hash here"), None);
    }
}
