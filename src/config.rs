use std::fs;
use std::path::PathBuf;

const DEFAULT_CONFIG: &str = "\
[core]
width = 50
initialized = false

[header]
auto_collapse = true
auto_collapse_timeout_ms = 5000
start_mode = \"expanded\"
";

pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("agentmux")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Ensure config file exists with section-based format.
/// Writes defaults on first start, or migrates old flat format.
pub fn ensure_config() {
    let path = config_path();
    let _ = fs::create_dir_all(config_dir());

    if !path.exists() {
        let _ = fs::write(path, DEFAULT_CONFIG);
        return;
    }

    // Migrate old flat format (no sections) to section-based format
    let content = fs::read_to_string(&path).unwrap_or_default();
    if !content.contains("[core]") {
        let _ = fs::write(&path, DEFAULT_CONFIG);
    }
}

/// Read a value from a specific section. Returns None if not found.
pub fn read_value(section: &str, key: &str) -> Option<String> {
    let content = fs::read_to_string(config_path()).ok()?;
    let section_header = format!("[{section}]");
    let mut in_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == section_header;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            if k.trim() == key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Write a value into a specific section. Creates the section if needed.
pub fn write_value(section: &str, key: &str, value: &str) {
    let path = config_path();
    let _ = fs::create_dir_all(config_dir());
    let content = fs::read_to_string(&path).unwrap_or_default();
    let section_header = format!("[{section}]");

    let mut result = Vec::new();
    let mut in_target_section = false;
    let mut key_written = false;
    let mut section_found = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            // Leaving previous section — if we were in target section and
            // didn't write the key yet, append it before this new section header.
            if in_target_section && !key_written {
                result.push(format!("{key} = {value}"));
                key_written = true;
            }
            in_target_section = trimmed == section_header;
            if in_target_section {
                section_found = true;
            }
            result.push(line.to_string());
            continue;
        }
        if in_target_section {
            if let Some((k, _)) = trimmed.split_once('=') {
                if k.trim() == key {
                    // Replace existing key
                    result.push(format!("{key} = {value}"));
                    key_written = true;
                    continue;
                }
            }
        }
        result.push(line.to_string());
    }

    // If we were in the target section at EOF and didn't write the key
    if in_target_section && !key_written {
        result.push(format!("{key} = {value}"));
        key_written = true;
    }

    // Section didn't exist — append it
    if !section_found {
        if !result.is_empty() && !result.last().unwrap().is_empty() {
            result.push(String::new());
        }
        result.push(section_header);
        result.push(format!("{key} = {value}"));
    }
    // key_written is only not set if section_found was false, handled above
    let _ = key_written;

    let _ = fs::write(path, result.join("\n") + "\n");
}
