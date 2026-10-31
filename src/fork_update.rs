//! `herdr update` for a build that carries the herdr-core mirror. The official
//! release would drop the mirror, so this installs the newest `core-mirror-*`
//! release of the fork instead, checked against its published SHA-256.

use std::path::Path;

use serde_json::Value;

const DEFAULT_REPO: &str = "andymai/herdr";
const REPO_OVERRIDE: &str = "HERDR_FORK_REPO";
const OFFICIAL_OVERRIDE: &str = "HERDR_FORK_SELF_UPDATE";
const TAG_PREFIX: &str = "core-mirror-";
const USAGE: &str = "usage: herdr update [--check]";

/// Handles `herdr update` and exits, unless `HERDR_FORK_SELF_UPDATE=1` asks
/// for the official updater.
pub(crate) fn intercept_self_update(args: &[String]) {
    if std::env::var(OFFICIAL_OVERRIDE).as_deref() == Ok("1") {
        return;
    }
    let code = match run(args) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    };
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<(), String> {
    let check_only = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        [flag] if flag == "--handoff" => {
            return Err(format!(
                "this build updates from its fork's releases without live handoff; \
                 run `herdr update`, then restart the server. {USAGE}"
            ))
        }
        _ => return Err(USAGE.to_owned()),
    };
    let asset = asset_name().ok_or("the fork publishes no build for this platform")?;
    let repo = std::env::var(REPO_OVERRIDE)
        .ok()
        .filter(|repo| !repo.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_owned());

    println!("checking {repo} for {TAG_PREFIX} releases...");
    let releases: Value = serde_json::from_slice(&fetch(&format!(
        "https://api.github.com/repos/{repo}/releases?per_page=30"
    ))?)
    .map_err(|err| format!("unreadable release list from {repo}: {err}"))?;
    let release = latest_fork_release(&releases)
        .ok_or_else(|| format!("{repo} has no {TAG_PREFIX} release yet"))?;
    let tag = release
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let binary_url = asset_url(release, asset)?;
    let checksum_url = asset_url(release, &format!("{asset}.sha256"))?;
    let expected = parse_checksum(&fetch(&checksum_url)?)
        .ok_or_else(|| format!("{tag} has an unreadable checksum"))?;

    let current_exe =
        std::env::current_exe().map_err(|err| format!("can't find this binary: {err}"))?;
    if crate::checksum::verify_sha256(&current_exe, &expected).is_ok() {
        println!("already up to date ({tag})");
        return Ok(());
    }
    if check_only {
        println!("{tag} is available; run `herdr update` to install it");
        return Ok(());
    }
    install(&current_exe, &binary_url, &expected)?;
    println!(
        "installed {tag} at {}. Restart the server to use it: `herdr server stop`, then `herdr`.",
        current_exe.display()
    );
    Ok(())
}

fn asset_name() -> Option<&'static str> {
    cfg!(all(target_os = "linux", target_arch = "x86_64")).then_some("herdr-linux-x86_64")
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let output = crate::noninteractive_process::curl_command()
        .args(["-sfL", "--max-time", "120"])
        .args(["-H", "Accept: application/vnd.github+json"])
        .arg(url)
        .output()
        .map_err(|err| format!("couldn't run curl: {err}"))?;
    if !output.status.success() {
        return Err(format!("download failed: {url}"));
    }
    Ok(output.stdout)
}

fn install(current_exe: &Path, url: &str, expected: &str) -> Result<(), String> {
    let dir = current_exe
        .parent()
        .ok_or("can't find this binary's directory")?;
    let tmp_path = dir.join(format!(".herdr-fork-update-{}.tmp", std::process::id()));
    let result = (|| {
        let status = crate::noninteractive_process::curl_command()
            .args(["-sfL", "--max-time", "300", "-o"])
            .arg(&tmp_path)
            .arg(url)
            .status()
            .map_err(|err| format!("couldn't run curl: {err}"))?;
        if !status.success() {
            return Err(format!("download failed: {url}"));
        }
        crate::checksum::verify_sha256(&tmp_path, expected)
            .map_err(|err| format!("downloaded build failed its checksum: {err}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
                .map_err(|err| format!("couldn't make the new build executable: {err}"))?;
        }
        std::fs::rename(&tmp_path, current_exe)
            .map_err(|err| format!("couldn't replace {}: {err}", current_exe.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

fn latest_fork_release(releases: &Value) -> Option<&Value> {
    releases
        .as_array()?
        .iter()
        .filter(|release| {
            !release
                .get("draft")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter(|release| {
            release
                .get("tag_name")
                .and_then(Value::as_str)
                .is_some_and(|tag| tag.starts_with(TAG_PREFIX))
        })
        .max_by_key(|release| {
            release
                .get("published_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
}

fn asset_url(release: &Value, name: &str) -> Result<String, String> {
    release
        .get("assets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|asset| asset.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|asset| asset.get("browser_download_url").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| format!("the newest fork release has no {name}"))
}

fn parse_checksum(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let digest = text.split_whitespace().next()?;
    (digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| digest.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn newest_published_fork_release_wins() {
        let releases = json!([
            {"tag_name": "core-mirror-v0.9.6-aaaa", "draft": true, "published_at": "2026-12-01T00:00:00Z"},
            {"tag_name": "v0.9.7", "draft": false, "published_at": "2026-12-02T00:00:00Z"},
            {"tag_name": "core-mirror-v0.9.5-bbbb", "draft": false, "published_at": "2026-10-30T00:00:00Z"},
            {"tag_name": "core-mirror-v0.9.6-cccc", "draft": false, "published_at": "2026-11-15T00:00:00Z"},
        ]);
        let release = latest_fork_release(&releases).unwrap();
        assert_eq!(release["tag_name"], "core-mirror-v0.9.6-cccc");
    }

    #[test]
    fn assets_are_found_by_name() {
        let release = json!({"assets": [
            {"name": "herdr-linux-x86_64", "browser_download_url": "https://example/bin"},
            {"name": "herdr-linux-x86_64.sha256", "browser_download_url": "https://example/sum"},
        ]});
        assert_eq!(
            asset_url(&release, "herdr-linux-x86_64.sha256").unwrap(),
            "https://example/sum"
        );
        assert!(asset_url(&release, "herdr-darwin-arm64").is_err());
    }

    #[test]
    fn checksum_files_read_like_sha256sum_output() {
        let digest = "A".repeat(64);
        assert_eq!(
            parse_checksum(format!("{digest}  herdr-linux-x86_64\n").as_bytes()),
            Some("a".repeat(64))
        );
        assert_eq!(parse_checksum(b"not a digest"), None);
    }
}
