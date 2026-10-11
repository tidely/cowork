//! Where the file tools' paths lead. This is the only place that turns a
//! path the model wrote into a file on the host, and a file back into what
//! the model is shown.
//!
//! | The model writes             | It means                                          |
//! |------------------------------|---------------------------------------------------|
//! | `<folder>` or `<folder>/a/b` | `a/b` in the project folder named `<folder>`      |
//! | `scratch://` or `scratch://a/b` | `a/b` in the thread's scratch folder           |
//! | an absolute path             | the same file, if it is inside one of the above   |
//! | `.`, `./`, `*`, or nothing   | the list of the places above                      |
//!
//! Nothing else resolves: not `~`, not a relative path without a folder
//! name, and no `..` anywhere. Folder names never contain `:` (see
//! [`crate::folder_name`]), so [`SCRATCH`] can't be mistaken for one.
//! Every path the tools show the model is in the first two forms, so it
//! can be passed straight back, and the host's own paths stay out of the
//! transcript collaborators see.
//!
//! A path resolves only if, with symbolic links resolved, it is still inside
//! the place it names, and not in one of the credential locations of
//! [`crate::settings::DENIED_PATHS`]. The project folders may be written only
//! when the project is writable; the scratch folder always may.

use std::path::{Component, Path, PathBuf};

use crate::{Project, settings};

/// How a path in the thread's scratch folder starts.
pub const SCRATCH: &str = "scratch://";

/// Inputs meaning the list of places rather than a path in one, as models
/// send them even when asked for a path.
const PLACES: &[&str] = &["", ".", "./", "*"];

/// Why a path could not be used, as the model is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// The model's arguments can't work, whatever is on disk.
    Invalid(String),
    NotFound(String),
    /// The path exists, or could, but the agent may not use it.
    Refused(String),
    /// Reading or writing failed.
    Failed(String),
}

impl std::fmt::Display for FileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::NotFound(message)
            | Self::Refused(message)
            | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for FileError {}

/// A place the file tools reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Root {
    Folder(String),
    Scratch,
}

impl Root {
    fn display(&self) -> &str {
        match self {
            Self::Folder(name) => name,
            Self::Scratch => SCRATCH,
        }
    }
}

struct Place {
    root: Root,
    /// Fully resolved, so it compares with other resolved paths.
    real: PathBuf,
}

/// The places a thread's file tools reach, as they are at one call.
pub(crate) struct Places {
    folders: Vec<Place>,
    scratch: Place,
    writable: bool,
    /// Resolved credential locations that exist.
    denied: Vec<PathBuf>,
}

/// What a path the model wrote names.
#[derive(Debug)]
pub(crate) enum Resolved {
    /// The list of places, for `.` and the like.
    Places,
    Path(Location),
}

/// A file or folder in one of the places, which may not exist yet.
#[derive(Clone, Debug)]
pub(crate) struct Location {
    pub(crate) root: Root,
    /// The components below the root, as the model is shown them.
    relative: Vec<String>,
    /// The resolved path on the host.
    pub(crate) real: PathBuf,
}

impl Location {
    /// The path as the model is shown it, and may write it.
    pub(crate) fn display(&self) -> String {
        let relative = self.relative.join("/");
        match &self.root {
            Root::Folder(name) if relative.is_empty() => name.clone(),
            Root::Folder(name) => format!("{name}/{relative}"),
            Root::Scratch => format!("{SCRATCH}{relative}"),
        }
    }

    pub(crate) fn is_root(&self) -> bool {
        self.relative.is_empty()
    }

    /// The entry `name` of this folder, at `real`.
    pub(crate) fn child(&self, name: String, real: PathBuf) -> Location {
        let mut relative = self.relative.clone();
        relative.push(name);
        Location {
            root: self.root.clone(),
            relative,
            real,
        }
    }
}

impl Places {
    /// The places of a thread with `project` and the scratch folder
    /// `scratch`, which need not exist yet. `home` locates the credentials
    /// in [`settings::DENIED_PATHS`].
    pub(crate) fn new(project: &Project, scratch: &Path, home: Option<&Path>) -> Self {
        let folders = project
            .folders
            .iter()
            .filter_map(|folder| {
                Some(Place {
                    root: Root::Folder(folder.name.clone()),
                    real: std::fs::canonicalize(&folder.path).ok()?,
                })
            })
            .collect();
        let scratch = Place {
            root: Root::Scratch,
            real: real_path(scratch).unwrap_or_else(|| scratch.to_owned()),
        };
        let denied = home
            .iter()
            .flat_map(|home| settings::DENIED_PATHS.iter().map(|path| home.join(path)))
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .collect();
        Self {
            folders,
            scratch,
            writable: project.writable,
            denied,
        }
    }

    /// The places, as the model writes them.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.places().map(|place| place.root.display())
    }

    /// Whether files at `location` may be written.
    pub(crate) fn writable(&self, location: &Location) -> bool {
        match location.root {
            Root::Folder(_) => self.writable,
            Root::Scratch => true,
        }
    }

    /// Whether `real`, a resolved path, is in `root` and not denied, as a
    /// folder's entries must be to be listed.
    pub(crate) fn reaches(&self, root: &Root, real: &Path) -> bool {
        self.place(root)
            .is_some_and(|place| real.starts_with(&place.real))
            && !self.is_denied(real)
    }

    /// What the path the model wrote names; see the module documentation.
    pub(crate) fn resolve(&self, input: &str) -> Result<Resolved, FileError> {
        let input = input.trim();
        if PLACES.contains(&input) {
            return Ok(Resolved::Places);
        }
        let location = if let Some(rest) = input.strip_prefix(SCRATCH) {
            self.below(&self.scratch, input, rest)?
        } else if input.starts_with('~') {
            return Err(FileError::Invalid(format!(
                "`{input}`: paths can't start with `~`. {}",
                self.how_to_write_paths()
            )));
        } else if Path::new(input).is_absolute() {
            self.absolute(input)?
        } else {
            let (name, rest) = input
                .split_once(std::path::is_separator)
                .unwrap_or((input, ""));
            let place = self
                .folders
                .iter()
                .find(|place| place.root == Root::Folder(name.to_owned()))
                .ok_or_else(|| {
                    FileError::Invalid(format!(
                        "`{name}` is not a project folder. {}",
                        self.how_to_write_paths()
                    ))
                })?;
            self.below(place, input, rest)?
        };
        if self.is_denied(&location.real) {
            return Err(FileError::Refused(format!(
                "{} is a credentials location the agent may not use.",
                location.display()
            )));
        }
        Ok(Resolved::Path(location))
    }

    /// `rest`, a path below `place` the model wrote as `input`.
    fn below(&self, place: &Place, input: &str, rest: &str) -> Result<Location, FileError> {
        let mut relative = Vec::new();
        let mut path = place.real.clone();
        for component in rest.split(std::path::is_separator) {
            if component.is_empty() || component == "." {
                continue;
            }
            // A component that is not a plain name, such as `..` or a
            // Windows drive like `C:`, would leave the place or replace it
            // when joined.
            let mut parts = Path::new(component).components();
            if !matches!(
                (parts.next(), parts.next()),
                (Some(Component::Normal(_)), None)
            ) {
                return Err(FileError::Invalid(format!(
                    "`{input}`: `{component}` is not allowed in a path."
                )));
            }
            relative.push(component.to_owned());
            path.push(component);
        }
        let real = real_path(&path).ok_or_else(|| {
            FileError::Failed(format!("`{input}` could not be resolved on the host."))
        })?;
        let location = Location {
            root: place.root.clone(),
            relative,
            real,
        };
        if !location.real.starts_with(&place.real) {
            return Err(FileError::Refused(format!(
                "{} leads outside {} through a symbolic link, which the file tools don't follow.",
                location.display(),
                place.root.display()
            )));
        }
        Ok(location)
    }

    /// An absolute path, if it is in one of the places.
    fn absolute(&self, input: &str) -> Result<Location, FileError> {
        let path = Path::new(input);
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(FileError::Invalid(format!(
                "`{input}`: `..` is not allowed in a path."
            )));
        }
        let outside = || {
            FileError::Refused(format!(
                "`{input}` is outside the project folders and the scratch folder. {}",
                self.how_to_write_paths()
            ))
        };
        let real = real_path(path).ok_or_else(outside)?;
        // The innermost place, should one hold another, as when the home
        // folder is a project folder.
        let place = self
            .places()
            .filter(|place| real.starts_with(&place.real))
            .max_by_key(|place| place.real.components().count())
            .ok_or_else(outside)?;
        let relative = real
            .strip_prefix(&place.real)
            .map_err(|_| outside())?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        Ok(Location {
            root: place.root.clone(),
            relative,
            real,
        })
    }

    fn places(&self) -> impl Iterator<Item = &Place> {
        self.folders.iter().chain(std::iter::once(&self.scratch))
    }

    fn place(&self, root: &Root) -> Option<&Place> {
        self.places().find(|place| &place.root == root)
    }

    fn is_denied(&self, real: &Path) -> bool {
        self.denied.iter().any(|denied| real.starts_with(denied))
    }

    fn how_to_write_paths(&self) -> String {
        let folders = self
            .folders
            .iter()
            .map(|place| format!("`{}`", place.root.display()))
            .collect::<Vec<_>>();
        if folders.is_empty() {
            format!("There are no project folders, so paths start with `{SCRATCH}`.")
        } else {
            format!(
                "Paths start with a project folder's name ({}) or with `{SCRATCH}`.",
                folders.join(", ")
            )
        }
    }
}

/// `path` with symbolic links resolved, as far as it exists, and the rest
/// appended, so a file about to be created resolves as its folder does.
fn real_path(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match std::fs::canonicalize(current) {
            Ok(mut real) => {
                real.extend(missing.iter().rev());
                return Some(real);
            }
            Err(_) => {
                missing.push(current.file_name()?.to_owned());
                current = current.parent()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectFolder, TestRoot};

    struct Fixture {
        root: TestRoot,
        places: Places,
    }

    /// A project folder `zed`, a scratch folder, and a home holding `.ssh`.
    fn fixture(writable: bool) -> Fixture {
        let root = TestRoot::new();
        let zed = root.0.join("zed");
        std::fs::create_dir_all(zed.join("src")).unwrap();
        std::fs::write(zed.join("src/main.rs"), "fn main() {}\n").unwrap();
        let home = root.0.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let project = Project {
            folders: vec![ProjectFolder {
                name: "zed".into(),
                path: zed,
            }],
            writable,
        };
        let places = Places::new(&project, &root.0.join("scratch"), Some(&home));
        Fixture { root, places }
    }

    fn location(places: &Places, input: &str) -> Location {
        match places.resolve(input) {
            Ok(Resolved::Path(location)) => location,
            other => panic!("{input} did not resolve to a path: {other:?}"),
        }
    }

    #[test]
    fn folder_names_and_scratch_lead_into_their_places() {
        let fixture = fixture(false);
        let main = location(&fixture.places, "zed/src/main.rs");
        assert_eq!(main.display(), "zed/src/main.rs");
        assert_eq!(
            main.real,
            std::fs::canonicalize(fixture.root.0.join("zed/src/main.rs")).unwrap()
        );
        assert!(location(&fixture.places, "zed/").is_root());
        assert_eq!(location(&fixture.places, "zed/./src").display(), "zed/src");

        let note = location(&fixture.places, "scratch://notes/a.md");
        assert_eq!(note.root, Root::Scratch);
        assert_eq!(note.display(), "scratch://notes/a.md");
        assert!(note.real.ends_with("scratch/notes/a.md"), "{note:?}");
        assert_eq!(location(&fixture.places, "scratch://").display(), SCRATCH);
    }

    #[test]
    fn absolute_paths_inside_a_place_are_shown_by_name() {
        let fixture = fixture(false);
        let absolute = fixture.root.0.join("zed").join("src").join("main.rs");
        let main = location(&fixture.places, absolute.to_str().unwrap());
        assert_eq!(main.display(), "zed/src/main.rs");

        let outside = fixture.root.0.join("elsewhere.txt");
        assert!(matches!(
            fixture.places.resolve(outside.to_str().unwrap()),
            Err(FileError::Refused(_))
        ));
    }

    #[test]
    fn dots_and_globs_mean_the_list_of_places() {
        let fixture = fixture(false);
        for input in PLACES {
            assert!(matches!(
                fixture.places.resolve(input),
                Ok(Resolved::Places)
            ));
        }
        assert_eq!(fixture.places.names().collect::<Vec<_>>(), ["zed", SCRATCH]);
    }

    #[test]
    fn paths_outside_the_places_do_not_resolve() {
        let fixture = fixture(false);
        for input in [
            "~/zed/src",
            "src/main.rs",
            "zed/../home",
            "zed/src/../../home",
            "scratch://../zed",
        ] {
            assert!(
                matches!(fixture.places.resolve(input), Err(FileError::Invalid(_))),
                "{input} resolved"
            );
        }
        let escape = fixture.root.0.join("zed").join("..").join("home");
        assert!(matches!(
            fixture.places.resolve(escape.to_str().unwrap()),
            Err(FileError::Invalid(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_inside_a_path_is_refused() {
        let fixture = fixture(false);
        assert!(matches!(
            fixture.places.resolve(r"zed\C:\Windows"),
            Err(FileError::Invalid(_))
        ));
    }

    #[test]
    fn credentials_are_refused_even_inside_a_place() {
        let root = TestRoot::new();
        let home = root.0.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "secret").unwrap();
        let project = Project {
            folders: vec![ProjectFolder {
                name: "home".into(),
                path: home.clone(),
            }],
            writable: true,
        };
        let places = Places::new(&project, &root.0.join("scratch"), Some(&home));
        assert!(matches!(
            places.resolve("home/.ssh/id_ed25519"),
            Err(FileError::Refused(_))
        ));
        let ssh = std::fs::canonicalize(home.join(".ssh")).unwrap();
        assert!(!places.reaches(&Root::Folder("home".into()), &ssh));
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_out_of_a_place_are_refused() {
        let fixture = fixture(false);
        std::os::unix::fs::symlink(fixture.root.0.join("home"), fixture.root.0.join("zed/link"))
            .unwrap();
        assert!(matches!(
            fixture.places.resolve("zed/link"),
            Err(FileError::Refused(_))
        ));
        assert!(matches!(
            fixture.places.resolve("zed/link/new.txt"),
            Err(FileError::Refused(_))
        ));
    }

    #[test]
    fn only_writable_projects_and_scratch_may_be_written() {
        let read_only = fixture(false);
        assert!(
            !read_only
                .places
                .writable(&location(&read_only.places, "zed/a"))
        );
        assert!(
            read_only
                .places
                .writable(&location(&read_only.places, "scratch://a"))
        );
        let writable = fixture(true);
        assert!(
            writable
                .places
                .writable(&location(&writable.places, "zed/a"))
        );
    }
}
