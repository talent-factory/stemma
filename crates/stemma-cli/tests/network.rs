//! Process tests of `remote`, `clone`, and `fetch` (STEMMA-305) against a local fixture: a
//! repository made by the program, served over HTTP on a loopback socket by the answering side of
//! the core (`stemma_core::serve::Answering`). The fixture is written for these tests with the
//! standard library and names no hub; the end-to-end tests against a real hub live in stemma-hub
//! (ADR-022).

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use common::{ok, operations, repository, run, run_failing, track_ok, write, Dir, ACTOR};
use stemma_core::protocol::record::HubKeys;
use stemma_core::protocol::{ErrorMessage, FetchRequest, FormerKeys, RepositoryId};
use stemma_core::repository::Repository;
use stemma_core::serve::{Answering, ListFacts};
use stemma_core::signature::PublicKey;
use stemma_core::transport::{OtherSide, Refusal};

/// Two Ed25519 public keys, made for tests; nobody here holds their private halves.
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIjwWEZ4603dDnTO8ilPzaYzYG5PnbKb6TxOzauTQ9du";
const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMt6BisKCOOMdkWowXYqXUrS1zKF4aE6QenzZ1SSB/Md";

const MESSAGE: &str = "application/vnd.stemma.message";
const ERROR: &str = "application/vnd.stemma.error";

/// What the fixture answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The answers of the answering side.
    Serve,
    /// 503 with `Retry-After: 7` to every request.
    Busy,
    /// 301 to another address.
    Redirect,
    /// 404 with the error `not-readable`.
    NotReadable,
}

struct State {
    key: &'static str,
    mode: Mode,
}

/// A repository with one commit on `main`, served at `http://127.0.0.1:<port>/ns/<name>`.
struct Hub {
    dir: Dir,
    address: String,
    state: Arc<Mutex<State>>,
}

impl Hub {
    fn start() -> Self {
        Self::start_on("main")
    }

    /// A hub whose one commit is on the branch `branch`.
    fn start_on(branch: &str) -> Self {
        let dir = Dir::new();
        ok(
            dir.path(),
            &["init", "--actor", ACTOR, "--initial-branch", branch],
        );
        write(dir.path(), "file.txt", "hello\n");
        track_ok(dir.path(), "file.txt");
        ok(dir.path(), &["commit", "--actor", ACTOR, "-m", "first"]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/ns/project", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            key: KEY,
            mode: Mode::Serve,
        }));
        let served: PathBuf = dir.path().to_path_buf();
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (key, mode) = {
                    let state = shared.lock().unwrap();
                    (state.key, state.mode)
                };
                let _ = answer(stream, &served, key, mode);
            }
        });
        Self {
            dir,
            address,
            state,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn set(&self, key: &'static str, mode: Mode) {
        let mut state = self.state.lock().unwrap();
        state.key = key;
        state.mode = mode;
    }

    /// A commit on `main` of the hub.
    fn commit(&self, text: &str) {
        write(self.path(), "file.txt", text);
        ok(self.path(), &["commit", "--actor", ACTOR, "-m", text]);
    }
}

/// Reads one request and answers it, then closes the connection.
fn answer(stream: TcpStream, served: &Path, key: &str, mode: Mode) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or("").to_owned(),
        parts.next().unwrap_or("").to_owned(),
    );
    let mut length = 0usize;
    let mut chunked = false;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
            if name.eq_ignore_ascii_case("transfer-encoding") && value.trim() == "chunked" {
                chunked = true;
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk)?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    let (status, extra, kind, bytes): (&str, String, &str, Vec<u8>) = match mode {
        Mode::Busy => (
            "503 Service Unavailable",
            "Retry-After: 7\r\n".into(),
            "",
            Vec::new(),
        ),
        Mode::Redirect => (
            "301 Moved Permanently",
            "Location: https://elsewhere.example/ns/project\r\n".into(),
            "",
            Vec::new(),
        ),
        Mode::NotReadable => (
            "404 Not Found",
            String::new(),
            ERROR,
            ErrorMessage::NotReadable.encode(),
        ),
        Mode::Serve => serve(served, key, &method, &path, &body),
    };
    let mut stream = stream;
    let mut head = format!(
        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\n",
        bytes.len()
    );
    if !kind.is_empty() {
        head.push_str(&format!("Content-Type: {kind}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

/// The answer of the answering side of the core to `GET .../refs` and `POST .../fetch`.
fn serve(
    served: &Path,
    key: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> (&'static str, String, &'static str, Vec<u8>) {
    let repository = Repository::open(served).unwrap();
    let keys = HubKeys::try_new(PublicKey::from_openssh(key).unwrap(), FormerKeys::none()).unwrap();
    let id: RepositoryId = "11".repeat(16).parse().unwrap();
    let entry = format!("b3:{}", "a1".repeat(32)).parse().unwrap();
    let facts = ListFacts::new(id, &keys, move |_| Some(entry));
    let side = Answering::new(&repository, facts);
    let result = match (method, path.rsplit('/').next()) {
        ("GET", Some("refs")) => side.refs().map(|list| list.encode()),
        ("POST", Some("fetch")) => {
            let request = FetchRequest::parse(body).unwrap();
            let mut stream = Vec::new();
            side.fetch(&request, &mut stream).map(|()| stream)
        }
        _ => return ("404 Not Found", String::new(), "", Vec::new()),
    };
    match result {
        Ok(bytes) => ("200 OK", String::new(), MESSAGE, bytes),
        Err(Refusal::Error(message)) => ("400 Bad Request", String::new(), ERROR, message.encode()),
        Err(_) => ("500 Internal Server Error", String::new(), "", Vec::new()),
    }
}

fn fingerprint(key: &str) -> String {
    PublicKey::from_openssh(key)
        .unwrap()
        .fingerprint()
        .to_string()
}

#[test]
fn a_clone_pins_the_key_fetches_and_makes_main_at_the_remote_reference() {
    let hub = Hub::start();
    let place = Dir::new();
    let result = run(
        place.path(),
        &["clone", "--actor", ACTOR, &hub.address, "copy"],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.contains(&fingerprint(KEY)),
        "{}",
        result.stdout
    );
    assert!(
        result
            .stdout
            .contains("The branch `main` starts at `remotes/origin/heads/main`."),
        "{}",
        result.stdout
    );
    let copy = place.path().join("copy");
    // The files of `main` are checked out, and `main` stands where the hub has it.
    assert_eq!(
        std::fs::read_to_string(copy.join("file.txt")).unwrap(),
        "hello\n"
    );
    let branches = ok(&copy, &["branch"]).stdout;
    assert!(branches.contains("main"), "{branches}");
    let list = ok(&copy, &["remote", "list"]).stdout;
    assert_eq!(
        list,
        format!("origin {} {}\n", hub.address, fingerprint(KEY))
    );
    // The entries: `init`, `fetch`, and the switch to `main`.
    let entries = operations(&copy);
    assert!(entries
        .iter()
        .any(|entry| entry.contains("\"operation\":\"fetch\"")));
}

#[test]
fn a_clone_refuses_an_unexpected_key_and_makes_nothing() {
    let hub = Hub::start();
    let place = Dir::new();
    let result = run(
        place.path(),
        &[
            "clone",
            "--actor",
            ACTOR,
            "--expect-key",
            &fingerprint(OTHER_KEY),
            &hub.address,
            "copy",
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(result.stderr.contains("was expected"), "{}", result.stderr);
    assert!(!place.path().join("copy").exists());
    // The expected key is taken, and the output says so.
    let result = run(
        place.path(),
        &[
            "clone",
            "--actor",
            ACTOR,
            "--expect-key",
            &fingerprint(KEY),
            "--json",
            &hub.address,
            "copy",
        ],
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    assert!(
        result.stdout.starts_with("{\"schema\":1,"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("\"expected\":true"),
        "{}",
        result.stdout
    );
    assert!(
        result.stdout.contains("\"branch\":\"main\""),
        "{}",
        result.stdout
    );
}

#[test]
fn remote_add_fetch_and_remove_write_their_entries_and_move_no_local_branch() {
    let hub = Hub::start();
    let local = repository();
    let added = run(
        local.path(),
        &[
            "remote",
            "add",
            "--actor",
            ACTOR,
            "--json",
            "origin",
            &hub.address,
        ],
    );
    assert_eq!(added.code, 0, "{}", added.stderr);
    assert!(
        added
            .stdout
            .contains(&format!("\"fingerprint\":\"{}\"", fingerprint(KEY))),
        "{}",
        added.stdout
    );
    assert!(operations(local.path())[0].contains("\"operation\":\"remote-add\""));
    // A second remote of that name is refused.
    let again = run(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    assert_eq!(again.code, 1);
    assert!(again.stderr.contains("exists already"), "{}", again.stderr);

    let fetched = ok(local.path(), &["fetch", "--actor", ACTOR]);
    assert!(
        fetched.stdout.contains("remotes/origin/heads/main  new at"),
        "{}",
        fetched.stdout
    );
    // Nothing new: no entry.
    let count = operations(local.path()).len();
    let nothing = ok(local.path(), &["fetch", "--actor", ACTOR, "origin"]);
    assert_eq!(nothing.stdout, "Nothing new from `origin`.\n");
    assert_eq!(operations(local.path()).len(), count);
    // A new commit on the hub moves the remote reference, and no local branch.
    hub.commit("second\n");
    let json = ok(local.path(), &["fetch", "--actor", ACTOR, "--json"]).stdout;
    assert!(
        json.contains("\"name\":\"remotes/origin/heads/main\""),
        "{json}"
    );
    assert!(!ok(local.path(), &["branch"]).stdout.contains("main"));
    // The remote reference is a revision that a local branch can start at.
    ok(
        local.path(),
        &[
            "switch",
            "--actor",
            ACTOR,
            "-c",
            "work",
            "remotes/origin/heads/main",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(local.path().join("file.txt")).unwrap(),
        "second\n"
    );

    let removed = ok(
        local.path(),
        &["remote", "remove", "--actor", ACTOR, "origin"],
    );
    assert_eq!(
        removed.stdout,
        "Removed the remote `origin` and its 1 remote reference.\n"
    );
    let entry = &operations(local.path())[0];
    assert!(entry.contains("\"operation\":\"remote-remove\""), "{entry}");
    assert!(
        entry.contains("\"name\":\"remotes/origin/heads/main\""),
        "{entry}"
    );
    assert_eq!(ok(local.path(), &["remote", "list"]).stdout, "");
    let gone = run(local.path(), &["fetch", "--actor", ACTOR]);
    assert_eq!(gone.code, 1);
    assert!(
        gone.stderr.contains("there is no remote `origin`"),
        "{}",
        gone.stderr
    );
    // `op restore` does not undo the removal.
    let number = entry
        .split("\"number\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap();
    let restore = run(local.path(), &["op", "restore", "--actor", ACTOR, number]);
    assert_eq!(restore.code, 1, "{}", restore.stderr);
    assert!(
        restore.stderr.contains("adds or removes a remote"),
        "{}",
        restore.stderr
    );
}

#[test]
fn a_changed_key_of_the_hub_is_refused_until_a_human_pins_it_again() {
    let hub = Hub::start();
    let local = repository();
    ok(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    hub.set(OTHER_KEY, Mode::Serve);
    let refused = run(local.path(), &["fetch", "--actor", ACTOR]);
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains(&format!(
            "stemma remote pin origin --expect-key {}",
            fingerprint(OTHER_KEY)
        )),
        "{}",
        refused.stderr
    );
    // A wrong fingerprint pins nothing; an agent may not pin; the human with the fingerprint does.
    let wrong = run(
        local.path(),
        &[
            "remote",
            "pin",
            "--actor",
            ACTOR,
            "--expect-key",
            &fingerprint(KEY),
            "origin",
        ],
    );
    assert_eq!(wrong.code, 1, "{}", wrong.stderr);
    let agent = run(
        local.path(),
        &[
            "remote",
            "pin",
            "--actor",
            "agent a1 model=m session=s1 sponsor=daniel",
            "--expect-key",
            &fingerprint(OTHER_KEY),
            "origin",
        ],
    );
    assert_eq!(agent.code, 1, "{}", agent.stderr);
    let pinned = ok(
        local.path(),
        &[
            "remote",
            "pin",
            "--actor",
            ACTOR,
            "--expect-key",
            &fingerprint(OTHER_KEY),
            "origin",
        ],
    );
    assert!(
        pinned.stdout.starts_with("Pinned the key"),
        "{}",
        pinned.stdout
    );
    ok(local.path(), &["fetch", "--actor", ACTOR]);
}

#[test]
fn a_busy_hub_is_code_6_and_a_redirect_and_a_missing_repository_are_named() {
    let hub = Hub::start();
    let local = repository();
    hub.set(KEY, Mode::Busy);
    let busy = run(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    assert_eq!(busy.code, 6, "{}", busy.stderr);
    assert!(
        busy.stderr.contains("retry after 7 s") && busy.stderr.contains("a later call can succeed"),
        "{}",
        busy.stderr
    );

    hub.set(KEY, Mode::Redirect);
    let redirect = run(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    assert_eq!(redirect.code, 1, "{}", redirect.stderr);
    assert!(
        redirect
            .stderr
            .contains("redirect to `https://elsewhere.example/ns/project`"),
        "{}",
        redirect.stderr
    );

    hub.set(KEY, Mode::NotReadable);
    let missing = run(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    assert_eq!(missing.code, 1, "{}", missing.stderr);
    assert!(
        missing.stderr.contains("or was renamed"),
        "{}",
        missing.stderr
    );
    // Nothing was written by any of them.
    assert_eq!(ok(local.path(), &["remote", "list"]).stdout, "");
    assert_eq!(operations(local.path()).len(), 1);
}

#[test]
fn the_arguments_of_the_network_commands_are_checked_by_the_parser() {
    let local = repository();
    for args in [
        &[
            "remote",
            "add",
            "--actor",
            ACTOR,
            "origin",
            "https://hub.example/project",
        ][..],
        &[
            "remote",
            "add",
            "--actor",
            ACTOR,
            "a/b",
            "https://hub.example/ns/project",
        ],
        &[
            "remote",
            "add",
            "--actor",
            ACTOR,
            "--expect-key",
            "MD5:00",
            "origin",
            "https://hub.example/ns/project",
        ],
        &["remote", "pin", "--actor", ACTOR, "origin"],
    ] {
        let result = run(local.path(), args);
        assert_eq!(result.code, 2, "{args:?}: {}", result.stderr);
    }
    // `http://` towards another host than a loopback address is refused before any request.
    let plain = run(
        local.path(),
        &[
            "remote",
            "add",
            "--actor",
            ACTOR,
            "origin",
            "http://hub.example/ns/project",
        ],
    );
    assert_eq!(plain.code, 1, "{}", plain.stderr);
    assert!(plain.stderr.contains("loopback"), "{}", plain.stderr);
    // A remote that asks for a sign-in needs a key.
    let sign_in = run(
        local.path(),
        &[
            "remote",
            "add",
            "--actor",
            ACTOR,
            "--sign-in",
            "origin",
            "https://hub.example/ns/project",
        ],
    );
    assert_eq!(sign_in.code, 1, "{}", sign_in.stderr);
    assert!(
        sign_in.stderr.contains("--key KEY_FILE"),
        "{}",
        sign_in.stderr
    );
}

#[test]
fn after_a_fetch_a_rebase_on_the_remote_reference_brings_main_along() {
    let hub = Hub::start();
    let place = Dir::new();
    ok(
        place.path(),
        &["clone", "--actor", ACTOR, &hub.address, "copy"],
    );
    let copy = place.path().join("copy");
    hub.commit("second\n");
    ok(&copy, &["fetch", "--actor", ACTOR]);
    // The fetch moved no local branch: the files are those of the first commit.
    assert_eq!(
        std::fs::read_to_string(copy.join("file.txt")).unwrap(),
        "hello\n"
    );
    ok(
        &copy,
        &["rebase", "--actor", ACTOR, "remotes/origin/heads/main"],
    );
    assert_eq!(
        std::fs::read_to_string(copy.join("file.txt")).unwrap(),
        "second\n"
    );
}

#[test]
fn a_clone_of_a_hub_without_main_makes_no_local_branch() {
    let hub = Hub::start_on("dev");
    let place = Dir::new();
    let result = ok(
        place.path(),
        &["clone", "--actor", ACTOR, "--json", &hub.address, "copy"],
    );
    assert!(
        result.stdout.contains("\"branch\":null"),
        "{}",
        result.stdout
    );
    let copy = place.path().join("copy");
    assert!(!ok(&copy, &["branch"]).stdout.contains("main"));
    let text = ok(
        place.path(),
        &["clone", "--actor", ACTOR, &hub.address, "again"],
    );
    assert!(
        text.stdout
            .contains("The hub has no `main`, so no local branch is made."),
        "{}",
        text.stdout
    );
}

#[test]
fn a_clone_into_an_existing_repository_is_refused_and_changes_nothing() {
    let hub = Hub::start();
    let place = repository();
    let before = operations(place.path());
    let result = run(
        place.path(),
        &["clone", "--actor", ACTOR, &hub.address, "."],
    );
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert_eq!(operations(place.path()), before);
    assert_eq!(ok(place.path(), &["remote", "list"]).stdout, "");
}

#[test]
fn a_clone_into_a_directory_that_is_not_empty_is_refused_and_leaves_it_alone() {
    let hub = Hub::start();
    let place = Dir::new();
    write(place.path(), "copy/notes.txt", "mine\n");
    let result = run(
        place.path(),
        &["clone", "--actor", ACTOR, &hub.address, "copy"],
    );
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("is not an empty directory"),
        "{}",
        result.stderr
    );
    assert!(!place.path().join("copy/.stemma").exists());
    let notes = std::fs::read_to_string(place.path().join("copy/notes.txt")).unwrap();
    assert_eq!(notes, "mine\n");
    // An empty directory is taken.
    std::fs::create_dir(place.path().join("empty")).unwrap();
    ok(
        place.path(),
        &["clone", "--actor", ACTOR, &hub.address, "empty"],
    );
    assert!(place.path().join("empty/.stemma").exists());
}

#[test]
fn a_taken_name_is_refused_before_the_hub_is_asked_for_a_sign_in() {
    let hub = Hub::start();
    let local = repository();
    ok(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    let before = operations(local.path());
    let result = run(
        local.path(),
        &[
            "remote",
            "add",
            "--actor",
            ACTOR,
            "--sign-in",
            "origin",
            &hub.address,
        ],
    );
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("the remote `origin` exists already"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(local.path()), before);
}

#[test]
fn a_fetch_from_a_remote_that_asks_for_a_sign_in_needs_a_key() {
    let hub = Hub::start();
    let local = repository();
    ok(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    let config = local.path().join(".stemma/remotes/origin");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("sign-in no\n"), "{text}");
    std::fs::write(&config, text.replace("sign-in no\n", "sign-in yes\n")).unwrap();
    let before = operations(local.path());
    let result = run(local.path(), &["fetch", "--actor", ACTOR]);
    assert_eq!(result.code, 1, "{}", result.stderr);
    assert!(
        result.stderr.contains("--key KEY_FILE"),
        "{}",
        result.stderr
    );
    assert_eq!(operations(local.path()), before);
}

#[test]
fn the_json_forms_of_list_remove_and_pin_name_their_fields() {
    let hub = Hub::start();
    let local = repository();
    ok(
        local.path(),
        &["remote", "add", "--actor", ACTOR, "origin", &hub.address],
    );
    let list = ok(local.path(), &["remote", "list", "--json"]).stdout;
    assert_eq!(
        list,
        format!(
            "{{\"schema\":1,\"remote\":\"origin\",\"address\":\"{}\",\
             \"repository\":\"{}\",\"fingerprint\":\"{}\",\"ca_file\":null,\
             \"sign_in\":false}}\n",
            hub.address,
            "11".repeat(16),
            fingerprint(KEY)
        )
    );
    // Pinning the key that is pinned already writes no entry.
    let count = operations(local.path()).len();
    let pin = ok(
        local.path(),
        &[
            "remote",
            "pin",
            "--actor",
            ACTOR,
            "--json",
            "--expect-key",
            &fingerprint(KEY),
            "origin",
        ],
    )
    .stdout;
    assert_eq!(
        pin,
        format!(
            "{{\"schema\":1,\"remote\":\"origin\",\"fingerprint\":\"{}\",\"entry\":null}}\n",
            fingerprint(KEY)
        )
    );
    assert_eq!(operations(local.path()).len(), count);
    let removed = ok(
        local.path(),
        &["remote", "remove", "--actor", ACTOR, "--json", "origin"],
    )
    .stdout;
    assert_eq!(
        removed,
        format!(
            "{{\"schema\":1,\"remote\":\"origin\",\"references\":0,\"entry\":{}}}\n",
            count + 1
        )
    );
}

#[test]
fn a_configuration_that_cannot_be_written_after_its_entry_is_code_3_and_a_repeat_finishes() {
    let hub = Hub::start();
    let local = repository();
    let remotes = local.path().join(".stemma/remotes");
    std::fs::create_dir_all(&remotes).unwrap();
    let args = ["remote", "add", "--actor", ACTOR, "origin", &hub.address];
    let failed = run_failing(local.path(), &args, &[("write", &remotes)]);
    assert_eq!(failed.code, 3, "{}", failed.stderr);
    assert!(
        failed.stderr.contains("run the command again"),
        "{}",
        failed.stderr
    );
    assert!(operations(local.path())[0].contains("\"operation\":\"remote-add\""));
    assert_eq!(ok(local.path(), &["remote", "list"]).stdout, "");
    ok(local.path(), &args);
    ok(local.path(), &["fetch", "--actor", ACTOR]);
    // The removal deletes the references and then fails on the configuration.
    let args = ["remote", "remove", "--actor", ACTOR, "origin"];
    let failed = run_failing(local.path(), &args, &[("write", &remotes)]);
    assert_eq!(failed.code, 3, "{}", failed.stderr);
    assert!(ok(local.path(), &["remote", "list"])
        .stdout
        .starts_with("origin "));
    let removed = ok(local.path(), &args);
    assert_eq!(
        removed.stdout,
        "Removed the remote `origin` and its 0 remote references.\n"
    );
    assert_eq!(ok(local.path(), &["remote", "list"]).stdout, "");
    ok(local.path(), &["op", "verify"]);
}

// The statements of a pull request on a hub (STEMMA-312). The fixture answers no request for
// statements: the HTTP mapping of those requests comes with STEMMA-326, so a submission here
// fails as one to a hub that does not take it, and the tests of the submission itself run
// against the transport in memory in the core.

/// A clone of the hub with the branch `feat` one commit ahead of `main`.
fn clone_with_feature(hub: &Hub, place: &Dir) -> PathBuf {
    ok(
        place.path(),
        &["clone", "--actor", ACTOR, &hub.address, "copy"],
    );
    let copy = place.path().join("copy");
    ok(&copy, &["branch", "--actor", ACTOR, "feat"]);
    ok(&copy, &["switch", "--actor", ACTOR, "feat"]);
    write(&copy, "feature.txt", "feature\n");
    track_ok(&copy, "feature.txt");
    ok(&copy, &["commit", "--actor", ACTOR, "-m", "feature"]);
    copy
}

/// The number in this clone of the only pull request, as `pr list` shows it.
fn listed_number(copy: &Path) -> String {
    let listed = ok(copy, &["pr", "list"]).stdout;
    listed.split_whitespace().next().unwrap().to_owned()
}

/// The digits of the name that `pr list` shows for the only pull request.
fn listed_id(copy: &Path) -> String {
    let listed = ok(copy, &["pr", "list"]).stdout;
    listed
        .split(" id ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("{listed}"))
        .to_owned()
}

#[test]
fn an_opening_for_a_hub_counts_in_the_clone_when_it_cannot_be_submitted() {
    let hub = Hub::start();
    let place = Dir::new();
    let copy = clone_with_feature(&hub, &place);
    let opened = run(
        &copy,
        &["pr", "open", "--actor", ACTOR, "feat", "--remote", "origin"],
    );
    // The entry is written, and the submission did not finish: exit 3.
    assert_eq!(opened.code, 3, "{}", opened.stderr);
    assert!(
        opened.stdout.contains("Opened pull request"),
        "{}",
        opened.stdout
    );
    assert!(
        opened.stderr.contains("counts in this clone")
            && opened.stderr.contains("stemma pr submit"),
        "{}",
        opened.stderr
    );
    let listed = ok(&copy, &["pr", "list"]).stdout;
    assert!(listed.contains(" hub\n"), "{listed}");
    let id = listed_id(&copy);
    assert_eq!(id.len(), 12, "{id}");
    // The pull request is named by its hash as well as by its number.
    let shown = ok(&copy, &["pr", "show", &id[..8]]).stdout;
    assert!(
        shown.contains(&format!("({id}), open")) && shown.contains(&"11".repeat(16)),
        "{shown}"
    );
    let shown = ok(&copy, &["pr", "show", &id, "--json"]).stdout;
    assert!(
        shown.contains(&format!("\"repository\":\"{}\"", "11".repeat(16)))
            && shown.contains("\"kind\":\"pr-opening\",\"in_clone\":true,\"held\":null"),
        "{shown}"
    );
    // `pr submit` makes another attempt and writes nothing.
    let before = operations(&copy).len();
    let submitted = run(&copy, &["pr", "submit", "--actor", ACTOR, &id]);
    assert_eq!(submitted.code, 1, "{}", submitted.stderr);
    assert_eq!(operations(&copy).len(), before);
    // An approval follows the opening to the hub, and counts here all the same.
    let approved = run(
        &copy,
        &["pr", "approve", "--actor", "human mia", &id, "--offline"],
    );
    assert_eq!(approved.code, 0, "{}", approved.stderr);
    assert!(
        approved.stdout.contains("`stemma pr submit"),
        "{}",
        approved.stdout
    );
    let listed = ok(&copy, &["pr", "list"]).stdout;
    assert!(listed.contains("approvals 1"), "{listed}");
}

#[test]
fn an_offline_opening_is_made_for_the_hub_and_submitted_later() {
    let hub = Hub::start();
    let place = Dir::new();
    let copy = clone_with_feature(&hub, &place);
    let opened = run(
        &copy,
        &[
            "pr",
            "open",
            "--actor",
            ACTOR,
            "feat",
            "--remote",
            "origin",
            "--offline",
        ],
    );
    assert_eq!(opened.code, 0, "{}", opened.stderr);
    assert!(
        opened
            .stdout
            .contains("counts in this clone; `stemma pr submit"),
        "{}",
        opened.stdout
    );
    let number = listed_number(&copy);
    let shown = ok(&copy, &["pr", "show", &number]).stdout;
    assert!(shown.contains("For the repository"), "{shown}");
    // A check for the hub, offline as well.
    let checked = run(
        &copy,
        &[
            "check",
            "record",
            "--actor",
            ACTOR,
            "feat",
            "--result",
            "pass",
            "--remote",
            "origin",
            "--offline",
        ],
    );
    assert_eq!(checked.code, 0, "{}", checked.stderr);
    let shown = ok(&copy, &["pr", "show", &number]).stdout;
    assert!(shown.contains("  check-result "), "{shown}");
}

#[test]
fn the_statements_of_a_pull_request_of_this_clone_only_go_to_no_hub() {
    let hub = Hub::start();
    let place = Dir::new();
    let copy = clone_with_feature(&hub, &place);
    ok(&copy, &["pr", "open", "--actor", ACTOR, "feat"]);
    let number = listed_number(&copy);
    let shown = ok(&copy, &["pr", "show", &number]).stdout;
    assert!(shown.contains("For this clone only."), "{shown}");
    let submitted = run(&copy, &["pr", "submit", "--actor", ACTOR, &number]);
    assert_eq!(submitted.code, 1, "{}", submitted.stderr);
    assert!(
        submitted.stderr.contains("counts in this clone only"),
        "{}",
        submitted.stderr
    );
    let before = operations(&copy).len();
    let approved = run(
        &copy,
        &[
            "pr",
            "approve",
            "--actor",
            "human mia",
            &number,
            "--remote",
            "origin",
        ],
    );
    assert_eq!(approved.code, 1, "{}", approved.stderr);
    assert_eq!(operations(&copy).len(), before, "nothing is written");
    let closed = run(
        &copy,
        &["pr", "close", "--actor", ACTOR, &number, "--offline"],
    );
    assert_eq!(closed.code, 1, "{}", closed.stderr);
    assert!(
        closed.stderr.contains("nothing is written"),
        "{}",
        closed.stderr
    );
    assert_eq!(operations(&copy).len(), before, "nothing is written");
    // `--offline` without a hub, and a name that is no name, are refused before anything.
    let refused = run(
        &copy,
        &["pr", "open", "--actor", ACTOR, "feat", "--offline"],
    );
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    for name in ["abc", "1a", "zzzzzzzzzz", "99"] {
        let refused = run(&copy, &["pr", "show", name]);
        assert_eq!(refused.code, 1, "{name}: {}", refused.stderr);
    }
    assert_eq!(operations(&copy).len(), before);
}

#[test]
fn after_its_entry_a_busy_hub_keeps_code_6_and_a_missing_key_refuses_before_the_entry() {
    let hub = Hub::start();
    let place = Dir::new();
    let copy = clone_with_feature(&hub, &place);
    // A remote that asks for a sign-in, without `--key`: nothing is written.
    let config = copy.join(".stemma/remotes/origin");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("sign-in no\n", "sign-in yes\n")).unwrap();
    let before = operations(&copy).len();
    let opened = run(
        &copy,
        &["pr", "open", "--actor", ACTOR, "feat", "--remote", "origin"],
    );
    assert_eq!(opened.code, 1, "{}", opened.stderr);
    assert!(
        opened.stderr.contains("--key KEY_FILE") && opened.stderr.contains("nothing is written"),
        "{}",
        opened.stderr
    );
    assert_eq!(operations(&copy).len(), before);
    // `--offline` needs no key.
    let opened = run(
        &copy,
        &[
            "pr",
            "open",
            "--actor",
            ACTOR,
            "feat",
            "--remote",
            "origin",
            "--offline",
        ],
    );
    assert_eq!(opened.code, 0, "{}", opened.stderr);
    std::fs::write(&config, text).unwrap();
    // A busy hub is no unreachable one: the approval is written, and the command ends with 6.
    hub.set(KEY, Mode::Busy);
    let number = listed_number(&copy);
    let before = operations(&copy).len();
    let approved = run(&copy, &["pr", "approve", "--actor", "human mia", &number]);
    assert_eq!(approved.code, 6, "{}", approved.stderr);
    assert!(
        approved.stderr.contains("counts in this clone")
            && approved.stderr.contains("a later call can succeed"),
        "{}",
        approved.stderr
    );
    assert_eq!(operations(&copy).len(), before + 1);
}
