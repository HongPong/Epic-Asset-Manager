//! fabdl - download an asset you own from your Fab library, for a chosen
//! engine version, into a folder. Memory-only login: no token is ever
//! written to disk.

use egs_api::api::types::download_manifest::{DownloadManifest, FileManifestList};
use egs_api::api::types::fab_library::{FabAsset, ProjectVersion};
use egs_api::EpicGames;
use futures_util::stream::{self, StreamExt};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const LOGIN_URL: &str = "https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3D34a02cf8f4414e29b15921876da36f9a%26responseType%3Dcode";
const CHUNK_CONCURRENCY: usize = 6;
/// Only ever send requests to these hosts (and subdomains of them).
const ALLOWED_HOST_SUFFIXES: [&str; 4] = [
    "epicgames.com",
    "fab.com",
    "fastly-edge.com",
    "epicgames.net",
];

const HELP: &str = "fabdl - download your own Fab assets for a chosen engine version

USAGE:
  fabdl library [FILTER] [--json FILE]
      List your Fab library (optionally filter by title text). With --json,
      write the full library manifest (ids, artifacts, engine versions,
      platforms, builds, titles, images) to FILE instead.
  fabdl list <FAB_URL_OR_UID>
      Show the engine versions / platforms available for one asset.
  fabdl get <FAB_URL_OR_UID> --engine UE_4.27 --out <DIR> [--platform Windows]
                              [--artifact <ID>] [--dry-run]
      Download one version. --dry-run lists files without downloading.

LOGIN:
  You are asked to open an Epic login page in your browser and paste the
  authorizationCode shown. The session lives in memory only and is logged
  out when the program ends. Set FABDL_CODE to skip the prompt.
";

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn fail(msg: impl AsRef<str>) -> ! {
    eprintln!("error: {}", msg.as_ref());
    std::process::exit(1)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return;
    }
    if let Err(e) = run(args).await {
        fail(e.to_string());
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

async fn run(args: Vec<String>) -> Res<()> {
    let cmd = args[0].as_str();
    let rest = &args[1..];
    // Validate arguments before asking for a login.
    let target = rest.first().filter(|a| !a.starts_with("--"));
    match cmd {
        "library" => {}
        "list" | "get" => {
            if target.is_none() {
                return Err("missing <FAB_URL_OR_UID>".into());
            }
            if cmd == "get" && flag(rest, "--out").is_none() {
                return Err("missing --out <DIR>".into());
            }
        }
        _ => return Err(format!("unknown command '{cmd}'\n\n{HELP}").into()),
    }

    let mut eg = login().await?;
    let result = match cmd {
        "library" => cmd_library(&mut eg, rest).await,
        "list" => cmd_list(&mut eg, target.unwrap()).await,
        _ => cmd_get(&mut eg, target.unwrap(), rest).await,
    };
    // Always end the Epic session so the token is useless afterwards.
    eg.logout().await;
    println!("Logged out of Epic session.");
    result
}

async fn login() -> Res<EpicGames> {
    let raw = match std::env::var("FABDL_CODE") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            println!("1. Open this page in your browser and sign in to Epic:\n\n   {LOGIN_URL}\n");
            println!("2. Copy the authorizationCode value (or the whole JSON) and paste it here.");
            print!("> ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line
        }
    };
    let code = extract_code(&raw).ok_or("could not find an authorization code in that input")?;
    let mut eg = EpicGames::new();
    if !eg.auth_code(None, Some(code)).await {
        return Err("login failed (code expired or already used - get a fresh one)".into());
    }
    let ud = eg.user_details();
    println!(
        "Logged in as {}",
        ud.display_name.as_deref().unwrap_or("(unknown)")
    );
    Ok(eg)
}

/// Accepts the bare code or the JSON blob Epic shows.
fn extract_code(raw: &str) -> Option<String> {
    let t = raw.trim();
    let candidate = if let Some(i) = t.find("authorizationCode") {
        let after = &t[i + "authorizationCode".len()..];
        after
            .trim_start_matches(|c: char| c == '"' || c == ':' || c.is_whitespace())
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or("")
            .to_string()
    } else {
        t.trim_matches('"').to_string()
    };
    if candidate.len() >= 16 && candidate.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(candidate)
    } else {
        None
    }
}

async fn load_library(eg: &mut EpicGames) -> Res<Vec<FabAsset>> {
    let account = eg
        .user_details()
        .account_id
        .ok_or("no account id after login")?;
    let lib = eg
        .try_fab_library_items(account)
        .await
        .map_err(|e| format!("could not load Fab library: {e:?}"))?;
    Ok(lib.results)
}

async fn cmd_library(eg: &mut EpicGames, args: &[String]) -> Res<()> {
    let items = load_library(eg).await?;
    let json_out = flag(args, "--json");
    let filter = args.first().filter(|a| !a.starts_with("--"));
    let f = filter.map(|s| s.to_lowercase());
    if let Some(path) = json_out {
        // Full library manifest: every field Fab returns for each item.
        let kept: Vec<&FabAsset> = items
            .iter()
            .filter(|a| f.as_ref().map_or(true, |f| a.title.to_lowercase().contains(f)))
            .collect();
        std::fs::write(&path, serde_json::to_string_pretty(&kept)?)?;
        println!("Wrote {} items to {path}", kept.len());
        return Ok(());
    }
    let mut n = 0;
    for a in &items {
        if f.as_ref().is_some_and(|f| !a.title.to_lowercase().contains(f)) {
            continue;
        }
        n += 1;
        let engines: Vec<String> = a
            .project_versions
            .iter()
            .flat_map(|p| p.engine_versions.iter().cloned())
            .collect();
        println!("{}  [{}]", a.title, engines.join(", "));
    }
    println!("{n} of {} items", items.len());
    Ok(())
}

/// Pulls a listing UID out of a Fab URL (or accepts a bare UID).
fn listing_uid(input: &str) -> String {
    let t = input.trim();
    let after = t.split("listings/").nth(1).unwrap_or(t);
    after
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

async fn find_asset(eg: &mut EpicGames, input: &str) -> Res<FabAsset> {
    let uid = listing_uid(input);
    let items = load_library(eg).await?;
    if let Some(a) = items
        .iter()
        .find(|a| a.url.to_lowercase().contains(&uid) || a.asset_id.to_lowercase() == uid)
    {
        return Ok(a.clone());
    }
    // Fall back to matching the listing's title against the library.
    if let Some(title) = eg.fab_listing(&uid).await.and_then(|l| l.title) {
        let t = title.to_lowercase();
        let hits: Vec<&FabAsset> = items
            .iter()
            .filter(|a| a.title.to_lowercase() == t)
            .collect();
        if hits.len() == 1 {
            return Ok(hits[0].clone());
        }
        if hits.len() > 1 {
            return Err(format!("title '{title}' matches {} library items; use `fabdl library` to inspect", hits.len()).into());
        }
    }
    Err("asset not found in your library (is it claimed/purchased on this account? try `fabdl library <text>`)".into())
}

fn print_versions(a: &FabAsset) {
    println!("\n{}  (asset id {})", a.title, a.asset_id);
    for p in &a.project_versions {
        println!(
            "  artifact {}\n    engines:   {}\n    platforms: {}\n    builds:    {}",
            p.artifact_id,
            p.engine_versions.join(", "),
            p.target_platforms.join(", "),
            p.build_versions
                .iter()
                .map(|b| format!("{} ({})", b.build_version, b.platform))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

async fn cmd_list(eg: &mut EpicGames, target: &str) -> Res<()> {
    let a = find_asset(eg, target).await?;
    print_versions(&a);
    Ok(())
}

fn pick_version<'a>(a: &'a FabAsset, args: &[String]) -> Res<&'a ProjectVersion> {
    if let Some(id) = flag(args, "--artifact") {
        return a
            .project_versions
            .iter()
            .find(|p| p.artifact_id == id)
            .ok_or_else(|| "no such artifact id".into());
    }
    let want = flag(args, "--engine").ok_or("pass --engine (see `fabdl list`) or --artifact")?;
    let norm = |s: &str| s.trim().to_uppercase().replace("UE_", "");
    let hits: Vec<&ProjectVersion> = a
        .project_versions
        .iter()
        .filter(|p| p.engine_versions.iter().any(|e| norm(e) == norm(&want)))
        .collect();
    match hits.len() {
        1 => Ok(hits[0]),
        0 => Err(format!("no version for engine '{want}'; run `fabdl list` to see what you own").into()),
        _ => Err("several artifacts match that engine; pick one with --artifact".into()),
    }
}

async fn cmd_get(eg: &mut EpicGames, target: &str, args: &[String]) -> Res<()> {
    let out = PathBuf::from(flag(args, "--out").ok_or("missing --out <DIR>")?);
    let dry = args.iter().any(|a| a == "--dry-run");
    let asset = find_asset(eg, target).await?;
    let pv = pick_version(&asset, args)?;
    let platform = flag(args, "--platform")
        .or_else(|| pv.target_platforms.iter().find(|p| p.as_str() == "Windows").cloned())
        .or_else(|| pv.target_platforms.first().cloned());
    println!(
        "Selected: {} / artifact {} / engines {} / platform {}",
        asset.title,
        pv.artifact_id,
        pv.engine_versions.join(", "),
        platform.as_deref().unwrap_or("(default)")
    );

    let infos = eg
        .fab_asset_manifest(
            &pv.artifact_id,
            &asset.asset_namespace,
            &asset.asset_id,
            platform.as_deref(),
        )
        .await
        .map_err(|e| format!("manifest request failed: {e:?}"))?;
    let info = infos.into_iter().next().ok_or("Epic returned no download info")?;
    let base = info
        .distribution_point_base_urls
        .first()
        .ok_or("no distribution point for this asset")?
        .clone();
    let point = info
        .get_distribution_point_by_base_url(&base)
        .ok_or("distribution point missing")?
        .clone();
    let manifest = eg
        .fab_download_manifest(info.clone(), &base)
        .await
        .map_err(|e| format!("manifest download failed: {e:?}"))?;

    // egs-api 0.14.0 does not append the signed query to chunk URLs, so
    // build them here from the base URL plus the manifest's signed query.
    let query = point.manifest_url.split_once('?').map(|(_, q)| q.to_string());
    let base_clean = base.split('?').next().unwrap_or(&base).trim_end_matches('/').to_string();
    let links = chunk_links(&manifest, &base_clean, query.as_deref())?;

    let files = &manifest.file_manifest_list;
    let total: u128 = files.iter().map(FileManifestList::size).sum();
    println!("{} files, {:.1} MiB", files.len(), total as f64 / 1_048_576.0);

    // Plan and validate every target path before touching the disk.
    std::fs::create_dir_all(&out)?;
    let root = std::fs::canonicalize(&out)?;
    let mut planned = Vec::new();
    for f in files {
        planned.push((safe_join(&root, &f.filename)?, f));
    }
    if dry {
        for (p, f) in &planned {
            println!("  {} ({} bytes)", p.strip_prefix(&root).unwrap_or(p).display(), f.size());
        }
        println!("Dry run: nothing downloaded.");
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut cache: HashMap<String, Vec<u8>> = HashMap::new();
    let mut ok = 0;
    for (i, (path, f)) in planned.iter().enumerate() {
        println!("[{}/{}] {}", i + 1, planned.len(), f.filename);
        if let Err(e) = write_file(&client, &links, &mut cache, path, f).await {
            eprintln!("  FAILED: {e}");
        } else {
            ok += 1;
        }
    }
    println!("Done: {ok}/{} files written to {}", planned.len(), root.display());
    if ok != planned.len() {
        return Err("some files failed (see above)".into());
    }
    Ok(())
}

fn chunk_dir(version: u128) -> &'static str {
    if version >= 15 {
        "ChunksV4"
    } else if version >= 6 {
        "ChunksV3"
    } else if version >= 3 {
        "ChunksV2"
    } else {
        "Chunks"
    }
}

fn chunk_links(
    m: &DownloadManifest,
    base: &str,
    query: Option<&str>,
) -> Res<HashMap<String, String>> {
    let dir = chunk_dir(m.manifest_file_version);
    let mut out = HashMap::new();
    for (guid, hash) in &m.chunk_hash_list {
        let group = m.data_group_list.get(guid).ok_or("chunk without data group")?;
        let mut url = format!("{base}/{dir}/{:02}/{:016X}_{}.chunk", group, hash, guid.to_uppercase());
        if let Some(q) = query {
            url.push('?');
            url.push_str(q);
        }
        check_host(&url)?;
        out.insert(guid.clone(), url);
    }
    Ok(out)
}

fn check_host(url: &str) -> Res<()> {
    let u = reqwest::Url::parse(url)?;
    let host = u.host_str().ok_or("URL without host")?.to_lowercase();
    let allowed = u.scheme() == "https"
        && ALLOWED_HOST_SUFFIXES
            .iter()
            .any(|s| host == *s || host.ends_with(&format!(".{s}")));
    if allowed {
        Ok(())
    } else {
        Err(format!("refusing to contact unexpected host '{host}'").into())
    }
}

/// Joins a manifest filename onto `root`, rejecting traversal.
fn safe_join(root: &Path, name: &str) -> Res<PathBuf> {
    let rel = Path::new(name);
    for c in rel.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("unsafe path in manifest: {name}").into()),
        }
    }
    if name.contains(':') || name.starts_with('\\') || name.contains("..\\") {
        return Err(format!("unsafe path in manifest: {name}").into());
    }
    let p = root.join(rel);
    if !p.starts_with(root) {
        return Err(format!("unsafe path in manifest: {name}").into());
    }
    Ok(p)
}

async fn fetch_chunk(client: &reqwest::Client, url: &str) -> Res<Vec<u8>> {
    let mut last = String::new();
    for attempt in 1..=3 {
        match client.get(url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => match r.bytes().await {
                Ok(b) => {
                    let chunk = egs_api::api::types::chunk::Chunk::from_vec(b.to_vec())
                        .ok_or("could not parse chunk")?;
                    return Ok(chunk.data);
                }
                Err(e) => last = e.to_string(),
            },
            Err(e) => last = e.without_url().to_string(),
        }
        eprintln!("  chunk retry {attempt}/3: {last}");
    }
    Err(format!("chunk download failed: {last}").into())
}

async fn write_file(
    client: &reqwest::Client,
    links: &HashMap<String, String>,
    cache: &mut HashMap<String, Vec<u8>>,
    path: &Path,
    f: &FileManifestList,
) -> Res<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Fetch the chunks this file still needs, a few at a time.
    let need: Vec<String> = f
        .file_chunk_parts
        .iter()
        .map(|p| p.guid.clone())
        .filter(|g| !cache.contains_key(g))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let fetched: Vec<(String, Res<Vec<u8>>)> = stream::iter(need)
        .map(|g| {
            let url = links.get(&g).cloned();
            async move {
                let r = match url {
                    Some(u) => fetch_chunk(client, &u).await,
                    None => Err("no URL for chunk".into()),
                };
                (g, r)
            }
        })
        .buffer_unordered(CHUNK_CONCURRENCY)
        .collect()
        .await;
    for (g, r) in fetched {
        cache.insert(g, r?);
    }

    let mut hasher = Sha1::new();
    let mut file = std::fs::File::create(path)?;
    for part in &f.file_chunk_parts {
        let data = cache.get(&part.guid).ok_or("missing chunk")?;
        let (a, b) = (part.offset as usize, (part.offset + part.size) as usize);
        let slice = data.get(a..b).ok_or("chunk too small for file part")?;
        hasher.update(slice);
        file.write_all(slice)?;
    }
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if !f.file_hash.is_empty() && !f.file_hash.chars().all(|c| c == '0') && got != f.file_hash {
        return Err(format!("hash mismatch (expected {}, got {got})", f.file_hash).into());
    }
    // Bound memory: chunks are mostly used once.
    if cache.len() > 256 {
        cache.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_code_from_json_or_bare() {
        let code = "0123456789abcdef0123456789abcdef";
        assert_eq!(extract_code(code).as_deref(), Some(code));
        let json = format!(r#"{{"redirectUrl":"x","authorizationCode":"{code}","sid":null}}"#);
        assert_eq!(extract_code(&json).as_deref(), Some(code));
        assert!(extract_code("nope").is_none());
    }

    #[test]
    fn parses_listing_uid() {
        assert_eq!(
            listing_uid("https://www.fab.com/listings/AB-12/?x=1"),
            "ab-12"
        );
        assert_eq!(listing_uid("ab-12"), "ab-12");
    }

    #[test]
    fn rejects_traversal() {
        let root = Path::new("/tmp/out");
        assert!(safe_join(root, "Content/a.uasset").is_ok());
        assert!(safe_join(root, "../x").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "C:\\x").is_err());
        assert!(safe_join(root, "a\\..\\..\\x").is_err());
    }

    #[test]
    fn only_epic_hosts() {
        assert!(check_host("https://egdownload.fastly-edge.com/a").is_ok());
        assert!(check_host("https://cdn.epicgames.com/a").is_ok());
        assert!(check_host("http://cdn.epicgames.com/a").is_err());
        assert!(check_host("https://evil.example.com/a").is_err());
        assert!(check_host("https://notepicgames.com/a").is_err());
    }
}
