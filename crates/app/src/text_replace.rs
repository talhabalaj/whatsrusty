//! macOS Text Replacements (System Settings → Keyboard → Text Replacements), applied
//! as you type like native text fields do: a shortcut followed by a space or
//! punctuation expands to its phrase. GPUI's text input doesn't get AppKit's automatic
//! replacement, so we read the user's list ourselves.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use gpui_kit::{App, Global};

#[derive(Default, Clone)]
pub struct TextReplacements(Arc<RwLock<HashMap<String, String>>>);

impl Global for TextReplacements {}

impl TextReplacements {
    /// Load in the background (it shells out to `defaults`/`plutil`).
    pub fn init(cx: &mut App) {
        let this = TextReplacements::default();
        cx.set_global(this.clone());
        cx.background_executor()
            .spawn(async move {
                let mut map = builtin();
                map.extend(load());
                log::info!("loaded {} text replacements", map.len());
                if let Ok(mut w) = this.0.write() {
                    *w = map;
                }
            })
            .detach();
    }

    /// If `text` ends with a word boundary right after a shortcut, the text with that
    /// shortcut expanded (keeping the boundary character).
    pub fn expand(cx: &App, text: &str) -> Option<String> {
        let this = cx.try_global::<TextReplacements>()?;
        let map = this.0.read().ok()?;
        expand_with(&map, text)
    }
}

/// Shortcuts WhatsApp expands itself (the user's own replacements win).
fn builtin() -> HashMap<String, String> {
    [("<3", "❤️"), ("</3", "💔")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | ',' | '!' | '?' | ';' | ':' | ')' | '"' | '\'')
}

pub fn expand_with(map: &HashMap<String, String>, text: &str) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    let boundary = text.chars().next_back().filter(|c| is_boundary(*c))?;
    let body = &text[..text.len() - boundary.len_utf8()];
    let start = body.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + body[i..].chars().next().map_or(1, char::len_utf8));
    let word = &body[start..];
    if word.is_empty() {
        return None;
    }
    let phrase = map.get(&word.to_lowercase())?;
    Some(format!("{}{phrase}{boundary}", &body[..start]))
}

fn load() -> HashMap<String, String> {
    use std::process::{Command, Stdio};
    let Ok(export) = Command::new("/usr/bin/defaults").args(["export", "-g", "-"]).output() else {
        return HashMap::new();
    };
    let Ok(mut plutil) = Command::new("/usr/bin/plutil")
        .args(["-extract", "NSUserDictionaryReplacementItems", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return HashMap::new();
    };
    if let Some(mut stdin) = plutil.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(&export.stdout);
    }
    let Ok(out) = plutil.wait_with_output() else { return HashMap::new() };
    let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        return HashMap::new();
    };
    items
        .iter()
        .filter(|i| i.get("on").and_then(|v| v.as_i64()).unwrap_or(1) != 0)
        .filter_map(|i| {
            let short = i.get("replace")?.as_str()?.trim();
            let phrase = i.get("with")?.as_str()?;
            (!short.is_empty() && !short.contains(char::is_whitespace)).then(|| (short.to_lowercase(), phrase.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_on_boundary_only() {
        let map: HashMap<_, _> = [("omw".to_string(), "On my way!".to_string())].into();
        assert_eq!(expand_with(&map, "omw ").as_deref(), Some("On my way! "));
        assert_eq!(expand_with(&map, "ok omw,").as_deref(), Some("ok On my way!,"));
        assert_eq!(expand_with(&map, "OMW ").as_deref(), Some("On my way! "));
        assert_eq!(expand_with(&map, "omw"), None);
        assert_eq!(expand_with(&map, "nomw "), None);
        assert_eq!(expand_with(&map, "صباح omw ").as_deref(), Some("صباح On my way! "));
    }
}
