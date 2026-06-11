use std::{fmt, path::PathBuf};

#[derive(Debug)]
pub enum ToolIoError {
    Path(PathError),
    Io(std::io::Error),
}

impl fmt::Display for ToolIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ToolIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<PathError> for ToolIoError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl From<std::io::Error> for ToolIoError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    HomeDirectoryUnavailable,
    CurrentDirectoryUnavailable(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeDirectoryUnavailable => {
                write!(f, "home directory environment variables are not set")
            }
            Self::CurrentDirectoryUnavailable(error) => {
                write!(f, "current directory is unavailable: {error}")
            }
        }
    }
}

impl std::error::Error for PathError {}

pub fn resolve_path(path: &str) -> Result<PathBuf, PathError> {
    let path = path.trim();
    let path = if path == "~" {
        home_dir()?
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home_dir()?.join(normalize_path_separators(rest))
    } else {
        PathBuf::from(normalize_path_separators(path))
    };

    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| PathError::CurrentDirectoryUnavailable(error.to_string()))
    }
}

fn home_dir() -> Result<PathBuf, PathError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            if drive.is_empty() || path.is_empty() {
                return None;
            }

            let mut home = drive;
            home.push(path);
            Some(PathBuf::from(home))
        })
        .ok_or(PathError::HomeDirectoryUnavailable)
}

fn normalize_path_separators(path: &str) -> String {
    if cfg!(windows) {
        path.replace('/', "\\")
    } else {
        path.replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_accepts_backslash_relative_paths() {
        let path = resolve_path("parent\\child").expect("path resolves");

        assert!(path.ends_with(PathBuf::from("parent").join("child")));
    }

    #[test]
    fn resolve_path_accepts_backslash_tilde_paths() {
        let path = resolve_path("~\\child").expect("path resolves");

        assert!(path.ends_with("child"));
        assert!(!path.to_string_lossy().contains('~'));
    }

    #[test]
    fn resolve_path_accepts_tilde_as_home() {
        let path = resolve_path("~").expect("path resolves");

        assert!(path.is_absolute());
        assert!(!path.to_string_lossy().contains('~'));
    }
}
