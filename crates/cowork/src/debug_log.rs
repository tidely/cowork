use std::{
    fs::OpenOptions,
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn enabled() -> bool {
    std::env::var_os("COWORK_DEBUG_LOG").is_some()
}

pub fn event(name: &str, fields: impl IntoIterator<Item = (&'static str, String)>) {
    let Some(path) = std::env::var_os("COWORK_DEBUG_LOG") else {
        return;
    };

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();

    let mut line = format!("{timestamp_ms} {name}");
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&sanitize(&value));
    }
    line.push('\n');

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn sanitize(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => sanitized.push_str("\\n"),
            '\r' => sanitized.push_str("\\r"),
            '\t' => sanitized.push_str("\\t"),
            _ => sanitized.push(ch),
        }
    }
    sanitized
}
