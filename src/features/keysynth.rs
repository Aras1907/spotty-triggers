//! Synthesize keyboard accelerators on the focused app via xdotool / wtype.
//!
//! Used to auto-paste (Ctrl+V) after a clipboard-history pick. Runs on the host
//! through `flatpak-spawn --host` when sandboxed.

use std::process::Command;

/// Synthesize a keyboard accelerator (e.g. "Ctrl+V") on the focused window.
/// Returns true if a backend (xdotool or wtype) was found and ran.
pub fn do_keybinding(keybinding: &str) -> bool {
    let Some((xdotool_key, wtype_args)) = accelerator_commands(keybinding) else {
        return false;
    };
    let script = format!(
        "if command -v xdotool >/dev/null 2>&1; then \
           xdotool key --clearmodifiers {}; \
         elif command -v wtype >/dev/null 2>&1; then \
           wtype {}; \
         else \
           exit 127; \
         fi",
        shell_quote(&xdotool_key),
        wtype_args
    );
    host_cmd()
        .args(["sh", "-lc", &script])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Type literal text (e.g. an emoji) into the focused window via `wtype`.
/// `wtype` talks to the compositor's virtual-keyboard protocol directly and,
/// unlike XTest-based tools (xdotool), doesn't trigger GNOME's "Remote
/// Desktop" input-control permission prompt. Returns false (without any
/// side effect) if `wtype` isn't installed, so the caller can fall back to
/// a clipboard paste.
pub fn do_type_text(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let quoted = shell_quote(text);
    let script = format!("command -v wtype >/dev/null 2>&1 && wtype {q}", q = quoted);
    host_cmd()
        .args(["sh", "-lc", &script])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn accelerator_commands(accel: &str) -> Option<(String, String)> {
    let mut modifiers = Vec::new();
    let mut key = None::<String>;
    for part in accel
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.to_lowercase().as_str() {
            "ctrl" | "control" | "primary" => modifiers.push("ctrl".to_string()),
            "shift" => modifiers.push("shift".to_string()),
            "alt" => modifiers.push("alt".to_string()),
            "super" | "meta" | "win" => modifiers.push("super".to_string()),
            _ => key = Some(synth_key_name(part)),
        }
    }
    let key = key?;
    let xdotool_key = if modifiers.is_empty() {
        key.clone()
    } else {
        format!("{}+{}", modifiers.join("+"), key)
    };

    let mut wtype_parts = Vec::new();
    for modifier in &modifiers {
        wtype_parts.push("-M".to_string());
        wtype_parts.push(wtype_modifier_name(modifier).to_string());
    }
    wtype_parts.push("-k".to_string());
    wtype_parts.push(key.clone());
    for modifier in modifiers.iter().rev() {
        wtype_parts.push("-m".to_string());
        wtype_parts.push(wtype_modifier_name(modifier).to_string());
    }
    let wtype_args = wtype_parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    Some((xdotool_key, wtype_args))
}

fn synth_key_name(part: &str) -> String {
    match part.to_lowercase().as_str() {
        "esc" | "escape" => "Escape".to_string(),
        "return" | "enter" => "Return".to_string(),
        "space" => "space".to_string(),
        "tab" => "Tab".to_string(),
        "backspace" => "BackSpace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "plus" | "+" => "plus".to_string(),
        "minus" | "-" => "minus".to_string(),
        "," => "comma".to_string(),
        "." => "period".to_string(),
        "/" => "slash".to_string(),
        other if other.len() == 1 => other.to_string(),
        _ => part.to_string(),
    }
}

fn wtype_modifier_name(modifier: &str) -> &str {
    match modifier {
        "super" => "logo",
        other => other,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn host_cmd() -> Command {
    if std::env::var("FLATPAK_ID").is_ok() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.arg("--host");
        cmd
    } else {
        Command::new("env")
    }
}
