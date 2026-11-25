//! Windows-safe file and folder names, shared by the engine (per-game folders) and the clip
//! library (rename). Pure string logic.

/// True for names Windows reserves as devices (`CON`, `NUL`, `COM1`...), with or without an
/// extension (`nul.txt` is still the NUL device), and for `.` and `..`.
pub fn is_reserved_name(name: &str) -> bool {
    let name = name.trim();
    if name == "." || name == ".." {
        return true;
    }
    let base = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base.len() == 4
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
}

/// A game's exe name as a single safe folder name: a trailing `.exe` (any case) is dropped,
/// characters Windows forbids become `_`, trailing dots and spaces go (Windows silently strips
/// them, so two names could collide), and a reserved or empty result gets a `_` prefix.
pub fn safe_folder_name(exe: &str) -> String {
    let stem = match exe.len().checked_sub(4).and_then(|at| exe.get(at..)) {
        Some(ext) if ext.eq_ignore_ascii_case(".exe") => &exe[..exe.len() - 4],
        _ => exe,
    };
    let cleaned: String = stem
        .chars()
        .map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']);
    if cleaned.is_empty() || is_reserved_name(cleaned) {
        format!("_{cleaned}")
    } else {
        cleaned.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_suffix_any_case_is_dropped() {
        assert_eq!(safe_folder_name("cs2.exe"), "cs2");
        assert_eq!(safe_folder_name("Game.EXE"), "Game");
        assert_eq!(safe_folder_name("é.exe"), "é");
    }

    #[test]
    fn dot_names_and_trailing_dots_are_neutralised() {
        assert_eq!(safe_folder_name("."), "_");
        assert_eq!(safe_folder_name(".."), "_");
        assert_eq!(safe_folder_name("game. . "), "game");
        assert_eq!(safe_folder_name(""), "_");
        assert_eq!(safe_folder_name(".exe"), "_");
    }

    #[test]
    fn reserved_device_names_are_prefixed() {
        for name in ["con.exe", "NUL", "Aux.exe", "prn", "COM1.exe", "lpt9"] {
            assert!(safe_folder_name(name).starts_with('_'), "{name}");
        }
        assert_eq!(safe_folder_name("COM10"), "COM10");
        assert_eq!(safe_folder_name("console.exe"), "console");
    }

    #[test]
    fn forbidden_characters_are_replaced() {
        assert_eq!(safe_folder_name("a:b/c.exe"), "a_b_c");
    }

    #[test]
    fn reserved_check_covers_extensions() {
        assert!(is_reserved_name("nul.txt"));
        assert!(is_reserved_name(".."));
        assert!(!is_reserved_name("ace"));
    }
}
