//! Offline game artwork discovery and background caching for the Home banner.
//!
//! Lookup priority (strictly offline, no network requests):
//! 1. **Steam local library cache** — find the Steam install from `HKCU\Software\Valve\Steam`
//!    (`SteamPath`) or the running executable's `steamapps` ancestor, parse `libraryfolders.vdf`
//!    and `appmanifest_*.acf` to resolve the `appid` (and pretty game title), then load
//!    `<Steam>/appcache/librarycache/<appid>/` hero/header art (both direct and hashed-subfolder
//!    layouts used by newer Steam clients).
//! 2. **Most recent clip thumbnail / screenshot** of that game.
//! 3. **Executable icon** cached via [`crate::ui::components::exe_icon`].
//! 4. **Fallback** — styled gradient badge with no image file.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::SystemTime;

use gpui::Context;
use zc_core::profile::exe_name;

use super::components::exe_icon;

/// Resolved artwork asset for a game banner, in priority order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtSource {
    /// Wide hero or header artwork from Steam's local `appcache/librarycache`.
    SteamHero(PathBuf),
    /// Most recent clip thumbnail or screenshot tagged with the game.
    ClipThumb(PathBuf),
    /// Extracted executable icon PNG cached by [`exe_icon`].
    ExeIcon(PathBuf),
    /// No image asset found; render the gradient fallback.
    Fallback,
}

/// Outcome of an offline artwork lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedArt {
    /// Best available artwork source.
    pub source: ArtSource,
    /// Pretty game title discovered from a Steam `appmanifest_*.acf`, if any.
    pub display_name: Option<String>,
}

impl Default for ResolvedArt {
    fn default() -> Self {
        Self { source: ArtSource::Fallback, display_name: None }
    }
}

/// Parameters needed to resolve artwork for one game off the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtRequest {
    /// Executable name or game tag (e.g. `"cs2.exe"`, `"Deadlock"`, `"Red Dead Redemption 2"`).
    pub game: String,
    /// Pre-generated thumbnail or screenshot path for the game's most recent clip, if available.
    pub clip_thumb: Option<PathBuf>,
    /// Directory where [`exe_icon`] caches extracted `.exe` icons.
    pub icons_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    game_lower: String,
    clip_thumb: Option<PathBuf>,
}

enum CacheEntry {
    Loading,
    Ready(ResolvedArt),
}

/// Asynchronous, non-blocking artwork cache owned by a GPUI view.
///
/// Cache misses spawn a lookup on GPUI's background executor and call `cx.notify()` once the
/// result lands; callers render the fallback immediately in the meantime.
#[derive(Default)]
pub struct ArtCache {
    /// Per `(game, clip_thumb)` lookup state.
    entries: HashMap<CacheKey, CacheEntry>,
    /// Games for which Steam artwork has already been found, keyed by lowercased game name, so a
    /// newly generated clip thumbnail does not re-trigger Steam disk discovery.
    steam_hits: HashMap<String, ResolvedArt>,
}

impl ArtCache {
    /// Returns cached artwork for `request` if ready, or kicks off a background lookup and
    /// returns `None` until it finishes.
    pub fn get_or_load<V: 'static>(
        &mut self,
        request: ArtRequest,
        of: fn(&mut V) -> &mut ArtCache,
        cx: &mut Context<V>,
    ) -> Option<&ResolvedArt> {
        let game_lower = request.game.trim().to_lowercase();
        if game_lower.is_empty() {
            return None;
        }
        if let Some(hit) = self.steam_hits.get(&game_lower) {
            return Some(hit);
        }
        let key =
            CacheKey { game_lower: game_lower.clone(), clip_thumb: request.clip_thumb.clone() };
        if self.entries.contains_key(&key) {
            return match self.entries.get(&key) {
                Some(CacheEntry::Ready(art)) => Some(art),
                _ => None,
            };
        }
        self.entries.insert(key.clone(), CacheEntry::Loading);
        cx.spawn(async move |this, cx| {
            let resolved =
                cx.background_executor().spawn(async move { resolve_offline(&request) }).await;
            let _ = this.update(cx, |view, cx| {
                let cache = of(view);
                if matches!(resolved.source, ArtSource::SteamHero(_)) {
                    cache.steam_hits.insert(game_lower, resolved.clone());
                }
                cache.entries.insert(key, CacheEntry::Ready(resolved));
                cx.notify();
            });
        })
        .detach();
        None
    }
}

/// Parsed fields of a Steam `appmanifest_<appid>.acf` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppManifest {
    /// Numeric Steam application identifier (e.g. `"730"`).
    pub appid: String,
    /// Display name from the manifest (e.g. `"Counter-Strike 2"`).
    pub name: String,
    /// Folder name under `steamapps/common/` (e.g. `"Counter-Strike Global Offensive"`).
    pub installdir: String,
}

/// Pure inputs for [`resolve_art_with`], separated from OS/registry probes so tests stay
/// deterministic and offline.
#[derive(Clone, Debug)]
pub struct ResolveInput {
    /// Game name or executable (e.g. `"Deadlock.exe"` or `"Deadlock"`).
    pub game: String,
    /// Full executable path of the running game, if known.
    pub exe_path: Option<PathBuf>,
    /// Main Steam installation directory (contains `appcache/librarycache` and
    /// `steamapps/libraryfolders.vdf`), if installed.
    pub steam_root: Option<PathBuf>,
    /// Most recent clip thumbnail or screenshot for this game, if present on disk.
    pub clip_thumb: Option<PathBuf>,
    /// Cached executable icon PNG for this game, if present on disk.
    pub exe_icon: Option<PathBuf>,
}

/// Runs the full offline lookup on a background thread using the host's Steam install and icon
/// cache.
fn resolve_offline(request: &ArtRequest) -> ResolvedArt {
    let exe_query = exe_query_name(&request.game);
    let exe_path = zc_win::process::exe_path(&exe_query).map(PathBuf::from);
    let steam_root = cached_steam_path();

    // Check Steam and clip thumb first; only capture an exe icon when needed as final fallback
    // (or reuse one that is already cached on disk).
    let existing_icon =
        exe_icon::cached(&request.icons_dir, &exe_query).filter(|p| is_non_empty_file(p));
    let mut input = ResolveInput {
        game: request.game.clone(),
        exe_path,
        steam_root,
        clip_thumb: request.clip_thumb.clone(),
        exe_icon: existing_icon,
    };
    let resolved = resolve_art_with(&input);
    if !matches!(resolved.source, ArtSource::Fallback) {
        return resolved;
    }
    if exe_icon::capture(&request.icons_dir, &exe_query)
        && let Some(captured) =
            exe_icon::cached(&request.icons_dir, &exe_query).filter(|p| is_non_empty_file(p))
    {
        input.exe_icon = Some(captured);
        return resolve_art_with(&input);
    }
    resolved
}

/// Deterministic offline artwork resolver following the strict priority order:
/// Steam librarycache -> most recent clip thumbnail -> exe icon -> Fallback.
#[must_use]
pub fn resolve_art_with(input: &ResolveInput) -> ResolvedArt {
    if let Some((art_path, title)) =
        resolve_steam_art(&input.game, input.exe_path.as_deref(), input.steam_root.as_deref())
    {
        return ResolvedArt { source: ArtSource::SteamHero(art_path), display_name: title };
    }

    // Even when no Steam artwork exists, keep any discovered Steam display name? No — if a
    // Steam manifest matched without cached art we still prefer the prettier title on fallbacks.
    let display_name =
        find_steam_manifest(&input.game, input.exe_path.as_deref(), input.steam_root.as_deref())
            .map(|m| m.name)
            .filter(|n| !n.is_empty());

    if let Some(thumb) = input.clip_thumb.as_ref().filter(|p| is_non_empty_file(p)) {
        return ResolvedArt { source: ArtSource::ClipThumb(thumb.clone()), display_name };
    }
    if let Some(icon) = input.exe_icon.as_ref().filter(|p| is_non_empty_file(p)) {
        return ResolvedArt { source: ArtSource::ExeIcon(icon.clone()), display_name };
    }
    ResolvedArt { source: ArtSource::Fallback, display_name }
}

/// Attempts to locate Steam librarycache banner artwork and the manifest's display name.
fn resolve_steam_art(
    game: &str,
    exe_path: Option<&Path>,
    steam_root: Option<&Path>,
) -> Option<(PathBuf, Option<String>)> {
    let manifest = find_steam_manifest(game, exe_path, steam_root)?;
    let cache_roots = librarycache_roots(exe_path, steam_root);
    for cache_dir in cache_roots {
        if let Some(path) = find_steam_library_art(&cache_dir, &manifest.appid) {
            let title = (!manifest.name.is_empty()).then_some(manifest.name);
            return Some((path, title));
        }
    }
    None
}

/// Candidate `appcache/librarycache` directories (main Steam root first, then exe's ancestor root).
fn librarycache_roots(exe_path: Option<&Path>, steam_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(root) = steam_root {
        roots.push(root.join("appcache").join("librarycache"));
    }
    if let Some((exe_root, _)) = exe_path.and_then(steam_root_and_installdir_from_exe) {
        let candidate = exe_root.join("appcache").join("librarycache");
        if !roots.iter().any(|r| paths_eq_ascii(r, &candidate)) {
            roots.push(candidate);
        }
    }
    roots
}

/// Finds the Steam `AppManifest` matching `game` (and optional `exe_path`) across all discovered
/// Steam library folders.
fn find_steam_manifest(
    game: &str,
    exe_path: Option<&Path>,
    steam_root: Option<&Path>,
) -> Option<AppManifest> {
    let exe_hint = exe_path.and_then(steam_root_and_installdir_from_exe);
    let mut library_roots: Vec<PathBuf> = Vec::new();

    if let Some((ref exe_root, _)) = exe_hint {
        push_unique_path(&mut library_roots, exe_root.clone());
    }
    if let Some(root) = steam_root {
        push_unique_path(&mut library_roots, root.to_path_buf());
        let vdf_path = root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(text) = std::fs::read_to_string(&vdf_path) {
            for lib in parse_library_folders(&text) {
                push_unique_path(&mut library_roots, lib);
            }
        }
    }

    let mut manifests: Vec<(PathBuf, AppManifest)> = Vec::new();
    for lib_root in &library_roots {
        let steamapps = lib_root.join("steamapps");
        let Ok(entries) = std::fs::read_dir(&steamapps) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !(name_str.starts_with("appmanifest_") && name_str.ends_with(".acf")) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(entry.path())
                && let Some(manifest) = parse_app_manifest(&content)
            {
                manifests.push((steamapps.clone(), manifest));
            }
        }
    }

    if manifests.is_empty() {
        return None;
    }

    // Priority 1: exact `installdir` derived from `.../steamapps/common/<installdir>/...`.
    if let Some((_, Some(ref installdir))) = exe_hint
        && let Some((_, m)) =
            manifests.iter().find(|(_, m)| m.installdir.eq_ignore_ascii_case(installdir))
    {
        return Some(m.clone());
    }

    let stem = clean_game_stem(game);
    if stem.is_empty() {
        return None;
    }

    // Priority 2: exact case-insensitive match against `installdir` or manifest `name`.
    if let Some((_, m)) = manifests
        .iter()
        .find(|(_, m)| m.installdir.eq_ignore_ascii_case(stem) || m.name.eq_ignore_ascii_case(stem))
    {
        return Some(m.clone());
    }

    // Priority 3: alphanumeric-normalized exact match (e.g. "RedDeadRedemption2").
    let norm = normalize_key(stem);
    if !norm.is_empty()
        && let Some((_, m)) = manifests
            .iter()
            .find(|(_, m)| normalize_key(&m.installdir) == norm || normalize_key(&m.name) == norm)
    {
        return Some(m.clone());
    }

    // Priority 4: shallow exe probe inside `<steamapps>/common/<installdir>` for games whose
    // binary name differs from the Steam title (e.g. `cs2.exe`, `RDR2.exe`, `aces.exe`).
    let exe_file = exe_query_name(game);
    for (steamapps, m) in &manifests {
        let base = steamapps.join("common").join(&m.installdir);
        if shallow_contains_exe(&base, &exe_file) {
            return Some(m.clone());
        }
    }

    None
}

/// Checks a handful of common top-level binary paths inside a game's install folder without
/// walking the whole directory tree.
fn shallow_contains_exe(install_root: &Path, exe_file: &str) -> bool {
    const SUBDIRS: [&str; 6] = ["", "bin", "win64", "game/bin/win64", "Binaries/Win64", "bin/x64"];
    SUBDIRS.iter().any(|sub| {
        let candidate = if sub.is_empty() {
            install_root.join(exe_file)
        } else {
            install_root.join(sub).join(exe_file)
        };
        candidate.is_file()
    })
}

/// Extracts `(steam_library_root, optional_installdir)` from an executable path inside
/// `.../steamapps/common/<installdir>/...`.
#[must_use]
pub fn steam_root_and_installdir_from_exe(exe_path: &Path) -> Option<(PathBuf, Option<String>)> {
    let components: Vec<Component<'_>> = exe_path.components().collect();
    for (i, comp) in components.iter().enumerate() {
        let Component::Normal(name) = comp else { continue };
        if !name.to_string_lossy().eq_ignore_ascii_case("steamapps") {
            continue;
        }
        if i == 0 {
            return None;
        }
        let mut root = PathBuf::new();
        for prefix in &components[..i] {
            root.push(prefix.as_os_str());
        }
        let installdir = match (components.get(i + 1), components.get(i + 2)) {
            (Some(Component::Normal(common)), Some(Component::Normal(dir)))
                if common.to_string_lossy().eq_ignore_ascii_case("common") =>
            {
                Some(dir.to_string_lossy().into_owned())
            }
            _ => None,
        };
        return Some((root, installdir));
    }
    None
}

/// Parses `"appid"`, `"name"`, and `"installdir"` from a Steam `appmanifest_*.acf` document.
#[must_use]
pub fn parse_app_manifest(acf: &str) -> Option<AppManifest> {
    let mut appid = None;
    let mut name = None;
    let mut installdir = None;
    let mut depth: usize = 0;

    for raw_line in acf.lines() {
        let line = raw_line.trim();
        if line == "{" {
            depth += 1;
            continue;
        }
        if line == "}" {
            depth = depth.saturating_sub(1);
            continue;
        }
        // Only read top-level fields inside `"AppState" { ... }` (depth == 1) so nested blocks
        // such as `"InstalledDepots"` or `"UserConfig"` cannot shadow them.
        if depth != 1 {
            continue;
        }
        let Some((key, value)) = parse_vdf_pair(line) else { continue };
        if key.eq_ignore_ascii_case("appid") {
            appid = Some(value);
        } else if key.eq_ignore_ascii_case("name") {
            name = Some(value);
        } else if key.eq_ignore_ascii_case("installdir") {
            installdir = Some(value);
        }
    }

    let appid = appid.filter(|s| !s.is_empty())?;
    let installdir = installdir.filter(|s| !s.is_empty())?;
    let name = name.unwrap_or_else(|| installdir.clone());
    Some(AppManifest { appid, name, installdir })
}

/// Extracts Steam library paths from `steamapps/libraryfolders.vdf`.
#[must_use]
pub fn parse_library_folders(vdf: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for raw_line in vdf.lines() {
        let Some((key, value)) = parse_vdf_pair(raw_line.trim()) else { continue };
        if key.eq_ignore_ascii_case("path") && !value.is_empty() {
            push_unique_path(&mut paths, PathBuf::from(value));
        }
    }
    paths
}

/// Parses one `"key" "value"` Valve KeyValues line, unescaping `\\` and `\"`.
fn parse_vdf_pair(line: &str) -> Option<(String, String)> {
    let (key, rest) = parse_quoted_token(line)?;
    let (value, _) = parse_quoted_token(rest.trim_start())?;
    Some((key, value))
}

fn parse_quoted_token(input: &str) -> Option<(String, &str)> {
    let rest = input.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\\' => {
                if let Some((_, escaped)) = chars.next() {
                    out.push(escaped);
                }
            }
            '"' => return Some((out, &rest[idx + 1..])),
            _ => out.push(ch),
        }
    }
    None
}

/// Finds the best landscape banner image for `appid` inside `<Steam>/appcache/librarycache`.
///
/// Supports all observed Steam cache layouts:
/// - `<cache>/<appid>/library_hero.jpg` (direct)
/// - `<cache>/<appid>/<hash>/library_hero.jpg` (newer hashed-subfolder layout)
/// - `library_header.jpg` / `header.jpg` in either `<appid>/` or `<appid>/<hash>/`
/// - Legacy flat `<cache>/<appid>_library_hero.jpg` / `<cache>/<appid>_header.jpg`
///
/// Explicitly skips blurred heroes (`library_hero_blur.jpg`), vertical capsules, logos, and
/// unnamed square icon hashes.
#[must_use]
pub fn find_steam_library_art(librarycache_dir: &Path, appid: &str) -> Option<PathBuf> {
    const PRIORITY_NAMES: [&str; 9] = [
        "library_hero.jpg",
        "library_hero.jpeg",
        "library_hero.png",
        "library_header.jpg",
        "library_header.jpeg",
        "library_header.png",
        "header.jpg",
        "header.jpeg",
        "header.png",
    ];

    let app_dir = librarycache_dir.join(appid);
    if app_dir.is_dir() {
        // Collect immediate subdirectories once (newer Steam stores each asset inside a 40-char
        // SHA-1 hash subfolder under `<appid>/`).
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&app_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();

        for filename in PRIORITY_NAMES {
            let direct = app_dir.join(filename);
            if is_non_empty_file(&direct) {
                return Some(direct);
            }
            // Check hashed subfolders; if more than one hash has the file, pick the newest.
            let best_in_sub = subdirs
                .iter()
                .map(|sub| sub.join(filename))
                .filter(|p| is_non_empty_file(p))
                .max_by_key(|p| {
                    std::fs::metadata(p)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH)
                });
            if let Some(found) = best_in_sub {
                return Some(found);
            }
        }
    }

    // Older flat Steam cache layout (`<appid>_library_hero.jpg`, `<appid>_header.jpg`).
    for suffix in ["library_hero.jpg", "library_hero.png", "header.jpg", "header.png"] {
        let flat = librarycache_dir.join(format!("{appid}_{suffix}"));
        if is_non_empty_file(&flat) {
            return Some(flat);
        }
    }

    None
}

/// Reads `HKCU\Software\Valve\Steam` (`SteamPath`) once and caches the result for the process.
fn cached_steam_path() -> Option<PathBuf> {
    static STEAM_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    STEAM_PATH.get_or_init(query_steam_path_from_registry).clone()
}

/// Queries `SteamPath` via `reg.exe` without flashing a console window, keeping `src/` free of
/// `unsafe` Win32 calls.
fn query_steam_path_from_registry() -> Option<PathBuf> {
    let mut cmd = Command::new("reg.exe");
    cmd.args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_reg_steam_path(&stdout)
}

/// Parses the `SteamPath` value from `reg.exe query` stdout.
#[must_use]
pub fn parse_reg_steam_path(stdout: &str) -> Option<PathBuf> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        let Some(rest) = strip_ascii_prefix(trimmed, "SteamPath") else { continue };
        let rest = rest.trim_start();
        let Some(after_type) = strip_ascii_prefix(rest, "REG_SZ")
            .or_else(|| strip_ascii_prefix(rest, "REG_EXPAND_SZ"))
        else {
            continue;
        };
        let value = after_type.trim();
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn strip_ascii_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// Strips directory prefix and trailing `.exe` for display or title matching.
#[must_use]
pub fn clean_game_stem(game: &str) -> &str {
    let name = exe_name(game).trim();
    name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name)
}

/// Ensures `game` ends with `.exe` when querying running processes or icon caches.
fn exe_query_name(game: &str) -> String {
    let name = exe_name(game).trim();
    if name.to_ascii_lowercase().ends_with(".exe") {
        name.to_owned()
    } else {
        format!("{name}.exe")
    }
}

fn normalize_key(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

fn is_non_empty_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

fn paths_eq_ascii(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

fn push_unique_path(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.iter().any(|existing| paths_eq_ascii(existing, &path)) {
        list.push(path);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn scratch_dir(label: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zc-art-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn write_bytes(path: &Path, data: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, data).expect("write fixture file");
    }

    #[test]
    fn parses_app_manifest_ignoring_nested_blocks() {
        let acf = r#"
"AppState"
{
	"appid"		"1422450"
	"universe"		"1"
	"name"		"Deadlock"
	"installdir"		"Deadlock"
	"UserConfig"
	{
		"name"		"should-not-overwrite"
	}
}
"#;
        let parsed = parse_app_manifest(acf).expect("valid manifest");
        assert_eq!(parsed.appid, "1422450");
        assert_eq!(parsed.name, "Deadlock");
        assert_eq!(parsed.installdir, "Deadlock");
    }

    #[test]
    fn parses_library_folders_vdf_with_escaped_backslashes() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"E:\\Steam"
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        let libs = parse_library_folders(vdf);
        assert_eq!(libs, vec![PathBuf::from(r"E:\Steam"), PathBuf::from(r"D:\SteamLibrary")]);
    }

    #[test]
    fn parses_reg_query_steam_path() {
        let stdout = "\r\nHKEY_CURRENT_USER\\Software\\Valve\\Steam\r\n    SteamPath    REG_SZ    e:/steam\r\n";
        assert_eq!(parse_reg_steam_path(stdout), Some(PathBuf::from("e:/steam")));
        assert_eq!(parse_reg_steam_path("ERROR: The system was unable to find the key."), None);
    }

    #[test]
    fn derives_steam_root_and_installdir_from_exe_path() {
        let exe = Path::new(r"E:\Steam\steamapps\common\Deadlock\game\bin\win64\deadlock.exe");
        let (root, dir) = steam_root_and_installdir_from_exe(exe).expect("steam exe");
        assert_eq!(root, PathBuf::from(r"E:\Steam"));
        assert_eq!(dir.as_deref(), Some("Deadlock"));

        let non_steam = Path::new(r"C:\Riot Games\VALORANT\live\VALORANT.exe");
        assert_eq!(steam_root_and_installdir_from_exe(non_steam), None);
    }

    #[test]
    fn steam_library_art_prefers_hero_and_handles_hashed_subfolders() {
        let root = scratch_dir("steam-layout");
        let cache = root.join("appcache").join("librarycache");

        // Direct layout: library_hero beats header and ignores blur/capsule/random icon hashes.
        let direct_app = cache.join("105600");
        write_bytes(&direct_app.join("library_hero_blur.jpg"), b"blur");
        write_bytes(&direct_app.join("header.jpg"), b"header");
        write_bytes(&direct_app.join("library_hero.jpg"), b"hero");
        assert_eq!(
            find_steam_library_art(&cache, "105600"),
            Some(direct_app.join("library_hero.jpg"))
        );

        // Hashed subfolder layout (newer Steam): finds hero inside 40-char hash dir.
        let hashed_app = cache.join("1422450");
        write_bytes(&hashed_app.join("f6da1420a173324d.jpg"), b"tiny-icon");
        write_bytes(&hashed_app.join("aa249c5e").join("library_header.jpg"), b"header");
        write_bytes(&hashed_app.join("e38dad54").join("library_hero.jpg"), b"hero");
        assert_eq!(
            find_steam_library_art(&cache, "1422450"),
            Some(hashed_app.join("e38dad54").join("library_hero.jpg"))
        );

        // Hashed header fallback when hero is absent; ignores unnamed hash icon jpgs.
        let header_only = cache.join("730");
        write_bytes(&header_only.join("8dbc71957312.jpg"), b"tiny-icon");
        write_bytes(&header_only.join("162664aa").join("header.jpg"), b"hdr");
        assert_eq!(
            find_steam_library_art(&cache, "730"),
            Some(header_only.join("162664aa").join("header.jpg"))
        );

        // Only a random hash icon (no hero/header) -> None so we don't stretch a 32x32 icon.
        let icon_only = cache.join("1042420");
        write_bytes(&icon_only.join("dadc80fcc935495943969e0d3cd90cae6c79d8ff.jpg"), b"icon");
        assert_eq!(find_steam_library_art(&cache, "1042420"), None);

        // Legacy flat layout fallback.
        write_bytes(&cache.join("4000_header.jpg"), b"flat-header");
        assert_eq!(find_steam_library_art(&cache, "4000"), Some(cache.join("4000_header.jpg")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fallback_priority_steam_then_clip_thumb_then_exe_icon_then_fallback() {
        let root = scratch_dir("priority");
        let steam = root.join("Steam");
        let secondary_lib = root.join("SteamLibrary");

        // Configure libraryfolders.vdf pointing at a secondary library where the manifest lives,
        // while librarycache lives in the main Steam install.
        let vdf = format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\"{}\"\n\t}}\n}}\n",
            steam.display().to_string().replace('\\', "\\\\"),
            secondary_lib.display().to_string().replace('\\', "\\\\"),
        );
        write_bytes(&steam.join("steamapps").join("libraryfolders.vdf"), vdf.as_bytes());
        write_bytes(
            &secondary_lib.join("steamapps").join("appmanifest_730.acf"),
            b"\"AppState\"\n{\n\t\"appid\"\t\"730\"\n\t\"name\"\t\"Counter-Strike 2\"\n\t\"installdir\"\t\"Counter-Strike Global Offensive\"\n}\n",
        );
        write_bytes(
            &secondary_lib
                .join("steamapps")
                .join("common")
                .join("Counter-Strike Global Offensive")
                .join("game")
                .join("bin")
                .join("win64")
                .join("cs2.exe"),
            b"MZ",
        );
        let hero = steam
            .join("appcache")
            .join("librarycache")
            .join("730")
            .join("9be13644")
            .join("library_hero.jpg");
        write_bytes(&hero, b"jpg-hero");

        let clip_thumb = root.join("thumbs").join("clip.jpg");
        write_bytes(&clip_thumb, b"jpg-thumb");
        let exe_icon = root.join("icons").join("cs2.exe.png");
        write_bytes(&exe_icon, b"png-icon");

        // 1. Steam hero wins even when clip thumb and exe icon exist (and works when idle via
        // shallow exe probe, returning Steam's display name).
        let res = resolve_art_with(&ResolveInput {
            game: "cs2".into(),
            exe_path: None,
            steam_root: Some(steam.clone()),
            clip_thumb: Some(clip_thumb.clone()),
            exe_icon: Some(exe_icon.clone()),
        });
        assert_eq!(res.source, ArtSource::SteamHero(hero.clone()));
        assert_eq!(res.display_name.as_deref(), Some("Counter-Strike 2"));

        // 2. When Steam art is missing, clip thumbnail wins over exe icon (preserving Steam title).
        std::fs::remove_file(&hero).expect("remove hero");
        let res = resolve_art_with(&ResolveInput {
            game: "cs2".into(),
            exe_path: None,
            steam_root: Some(steam.clone()),
            clip_thumb: Some(clip_thumb.clone()),
            exe_icon: Some(exe_icon.clone()),
        });
        assert_eq!(res.source, ArtSource::ClipThumb(clip_thumb));
        assert_eq!(res.display_name.as_deref(), Some("Counter-Strike 2"));

        // 3. When clip thumb is absent, exe icon wins over Fallback.
        let res = resolve_art_with(&ResolveInput {
            game: "Valorant.exe".into(),
            exe_path: None,
            steam_root: Some(steam.clone()),
            clip_thumb: None,
            exe_icon: Some(exe_icon.clone()),
        });
        assert_eq!(res.source, ArtSource::ExeIcon(exe_icon));
        assert_eq!(res.display_name, None);

        // 4. When nothing exists on disk, return Fallback.
        let res = resolve_art_with(&ResolveInput {
            game: "UnknownGame.exe".into(),
            exe_path: None,
            steam_root: Some(steam),
            clip_thumb: Some(root.join("missing.jpg")),
            exe_icon: Some(root.join("missing.png")),
        });
        assert_eq!(res.source, ArtSource::Fallback);
        assert_eq!(res.display_name, None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
