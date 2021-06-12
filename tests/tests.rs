use fcp::{self, filesystem as fs};
use lazy_static::lazy_static;
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use serde::Deserialize;
use std::env;
use std::io::prelude::*;
use std::io::SeekFrom;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};
use std::str;
use std::sync::Once;

lazy_static! {
    static ref FIXTURES_DIR: PathBuf = PathBuf::from("fixtures");
    static ref HYDRATED_DIR: PathBuf = FIXTURES_DIR.join("hydrated");
    static ref COPIES_DIR: PathBuf = FIXTURES_DIR.join("copies");
}

static INIT: Once = Once::new();

/// Must be called at the beginning of each test case
pub fn initialize() {
    INIT.call_once(|| {
        if !HYDRATED_DIR.exists() {
            fs::create_dir(&*HYDRATED_DIR, 0o777).unwrap();
        }
        if !COPIES_DIR.exists() {
            fs::create_dir(&*COPIES_DIR, 0o777).unwrap();
        }
    });
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
enum FileKind {
    #[serde(rename = "file")]
    Regular {
        size: u64,
    },
    #[serde(rename = "link")]
    Symlink {
        target: PathBuf,
    },
    Directory {
        contents: Vec<FileStub>,
    },
    Fifo {
        size: u64,
    },
    Socket {},
}

#[derive(Debug, Deserialize)]
struct FileStub {
    name: String,
    mode: u32,
    #[serde(flatten)]
    kind: FileKind,
}

fn remove(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
        .unwrap();
    }
}

fn hydrate_fixture(filename: &str) {
    let fixture_path = FIXTURES_DIR.join(filename);
    let output_path = HYDRATED_DIR.join(filename.strip_suffix(".json").unwrap());
    // We check if the file exists like this instead of via Path::exists
    // because we consider broken symlinks as still existing.
    if let Ok(output_meta) = fs::symlink_metadata(&output_path) {
        let fixture_modification_time = fs::symlink_metadata(&fixture_path)
            .unwrap()
            .modified()
            .unwrap();
        let output_creation_time = output_meta.created().unwrap();
        if fixture_modification_time < output_creation_time {
            return; // Fixture has already been hydrated, do nothing
        }
        remove(&output_path);
    }

    let mut files = serde_json::Deserializer::from_reader(fs::open(&fixture_path).unwrap());
    files.disable_recursion_limit();
    files
        .into_iter::<Vec<FileStub>>()
        .flat_map(Result::unwrap)
        .for_each(hydrate_file);
}

fn hydrate_file(file: FileStub) {
    let path = HYDRATED_DIR.join(file.name);
    let FileStub { mode, .. } = file;
    match file.kind {
        FileKind::Regular { size } => {
            let mut file = fs::create(path, mode).unwrap();
            let metadata = file.metadata().unwrap();
            if metadata.len() < size {
                file.seek(SeekFrom::End(0)).unwrap();
                let mut random = fs::open("/dev/random").unwrap();
                let mut remaining: usize = (size - metadata.len()) as usize;
                let mut buffer = [0u8; 4096];
                while remaining > 0 {
                    let bytes_to_process = std::cmp::min(remaining as usize, buffer.len());
                    let slice = &mut buffer[..bytes_to_process];
                    random.read_exact(slice).unwrap();
                    file.write_all(slice).unwrap();
                    remaining -= bytes_to_process;
                }
            }
        }
        FileKind::Symlink { target } => fs::symlink(HYDRATED_DIR.join(target), path).unwrap(),
        FileKind::Fifo { .. } => fs::mkfifo(path, PermissionsExt::from_mode(mode)).unwrap(),
        FileKind::Directory { contents } => {
            fs::create_dir(path, mode).unwrap();
            contents.into_par_iter().for_each(hydrate_file);
        }
        FileKind::Socket {} => {
            UnixListener::bind(path).unwrap();
        }
    }
}

fn diff(filename: &str) -> ExitStatus {
    let filename = filename.strip_suffix(".json").unwrap();
    Command::new("diff")
        .args(&[
            "-rq",
            "--no-dereference",
            HYDRATED_DIR.join(filename).to_str().unwrap(),
            COPIES_DIR.join(filename).to_str().unwrap(),
        ])
        .status()
        .unwrap()
}

fn fcp_executable_path() -> PathBuf {
    let mut executable = env::current_exe().unwrap();
    executable.pop();
    executable.pop();
    executable.push(format!("fcp{}", env::consts::EXE_SUFFIX));
    executable
}

fn copy_fixture(filename: &str) -> Output {
    let filename = filename.strip_suffix(".json").unwrap();
    let output = COPIES_DIR.join(filename);
    remove(&output);
    Command::new(fcp_executable_path())
        .args(&[
            HYDRATED_DIR.join(filename).to_str().unwrap().to_string(),
            output.to_str().unwrap().to_string(),
        ])
        .output()
        .unwrap()
}

macro_rules! make_test {
    ($(#[$attributes:meta])*
     $test_name:ident) => {
        #[test]
        $(#[$attributes])*
        fn $test_name() {
            initialize();
            let fixture_file = concat!(stringify!($test_name), ".json");
            hydrate_fixture(fixture_file);
            let result = copy_fixture(fixture_file);
            assert!(result.status.success());
            assert_eq!(str::from_utf8(&result.stderr).unwrap(), "");
            assert!(diff(fixture_file).success());
        }
    };
}

make_test!(regular_file);
make_test!(symlink);
make_test!(empty_directory);
make_test!(simple_directory);
make_test!(deep_directory);
make_test!(
    #[ignore]
    linux
);

#[test]
fn socket() {
    initialize();
    let fixture_file = "socket.json";
    hydrate_fixture(fixture_file);
    let result = copy_fixture(fixture_file);
    assert!(!result.status.success());
    assert!(str::from_utf8(&result.stderr)
        .unwrap()
        .contains("sockets cannot be copied"));
}

#[test]
fn fifo() {
    initialize();
    let fixture_file = "fifo.json";
    hydrate_fixture(fixture_file);
    let result = copy_fixture(fixture_file);
    assert!(result.status.success());
    let file_type =
        fs::file_type(&COPIES_DIR.join(fixture_file.strip_suffix(".json").unwrap())).unwrap();
    assert!(matches!(file_type, fs::FileType::Fifo(..)))
}

#[test]
fn character_device() {
    initialize();
    let output_path = COPIES_DIR.join("character_device");
    remove(&output_path);
    let contents = "Hello world\r";
    let result = Command::new("tests/character_device.exp")
        .args(&[
            fcp_executable_path().to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
            contents.to_string(),
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(str::from_utf8(&result.stderr).unwrap(), "");
    assert!(output_path.exists());
    let mut output_file = fs::open(output_path).unwrap();
    let mut output_contents = Vec::with_capacity(contents.len());
    output_file.read_to_end(&mut output_contents).unwrap();
    assert_eq!(
        str::from_utf8(&output_contents).unwrap(),
        contents.replace('\r', "\n")
    );
}

#[test]
fn too_few_arguments() {
    initialize();
    let mut result = Command::new(fcp_executable_path()).output().unwrap();
    assert!(!result.status.success());
    result = Command::new(fcp_executable_path())
        .arg("source")
        .output()
        .unwrap();
    assert!(!result.status.success());
}

#[test]
fn source_does_not_exist() {
    initialize();
    let destination = COPIES_DIR.join("destination");
    let source = "nonexistent_source";
    remove(&destination);
    let result = Command::new(fcp_executable_path())
        .args(&[source, destination.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(str::from_utf8(&result.stderr).unwrap().contains(source));
    assert!(!destination.exists());
}

#[test]
// A directory containing one.txt, two.txt, and three.txt
// where two.txt is inaccessible due to its permissions. We want
// to ensure that the error in copying two.txt is reported, but that
// the other files are still copied successfully.
fn partial_directory() {
    initialize();
    let fixture_file = "partial_directory.json";
    hydrate_fixture(fixture_file);
    let mut result = copy_fixture(fixture_file);
    assert!(!result.status.success());
    assert!(str::from_utf8(&result.stderr)
        .unwrap()
        .contains("partial_directory/two.txt"));
    for file in &["one.txt", "three.txt"] {
        result = Command::new("diff")
            .args(&[
                "-q",
                HYDRATED_DIR
                    .join("partial_directory")
                    .join(file)
                    .to_str()
                    .unwrap(),
                COPIES_DIR
                    .join("partial_directory")
                    .join(file)
                    .to_str()
                    .unwrap(),
            ])
            .output()
            .unwrap();
        assert!(result.status.success());
    }
}

#[test]
fn copy_into() {
    initialize();
    let empty_path = COPIES_DIR.join("empty");
    let temp_dir_path = COPIES_DIR.join("temp");
    remove(&empty_path);
    remove(&temp_dir_path);
    fs::create(&empty_path, 0o777).unwrap();
    fs::create_dir(&temp_dir_path, 0o777).unwrap();
    let result = Command::new(fcp_executable_path())
        .args(&[
            empty_path.to_str().unwrap(),
            temp_dir_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(str::from_utf8(&result.stderr).unwrap(), "");
    assert!(temp_dir_path.join("empty").exists());
}

#[test]
fn copy_many_into() {
    initialize();
    let empty_names = ["empty1", "empty2", "empty3"];
    let empty_paths = empty_names
        .iter()
        .map(|filename| COPIES_DIR.join(filename))
        .collect::<Box<_>>();
    let temp_dir_path = COPIES_DIR.join("temp_many");
    for path in empty_paths.iter() {
        remove(path);
        fs::create(path, 0o777).unwrap();
    }
    remove(&temp_dir_path);
    fs::create_dir(&temp_dir_path, 0o777).unwrap();
    let result = Command::new(fcp_executable_path())
        .args(empty_paths.iter())
        .arg(&temp_dir_path)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(str::from_utf8(&result.stderr).unwrap(), "");
    for name in &empty_names {
        assert!(temp_dir_path.join(name).exists());
    }
}
