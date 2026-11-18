//! Several components of one type updating in one run: their installs, with
//! their backups and registry writes, run one at a time, so none of them
//! loses its registry entry to another's rewrite of the shared file.

#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]

mod common;

use std::fmt::Write as _;

use common::{EXTRAS_BARRIER_TIMEOUT, ExtraEntry, FakeStore, Listing, NEW_VERSION, Sandbox};
use libplasmoid_updater::{Config, Reporter, update};

struct Quiet;

impl Reporter for Quiet {}

const COMIC_CATEGORY: u16 = 155;

/// Four comic plugins sharing `comic.knsregistry`: `(store id, name, package id)`.
const COMICS: [(u64, &str, &str); 4] = [
    (999_041, "Serial Comic A", "org.example.serialcomica"),
    (999_042, "Serial Comic B", "org.example.serialcomicb"),
    (999_043, "Serial Comic C", "org.example.serialcomicc"),
    (999_044, "Serial Comic D", "org.example.serialcomicd"),
];

const ARCHIVE_A: &[(&str, &str)] = &[(
    "org.example.serialcomica/metadata.json",
    r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.serialcomica","Name":"Serial Comic A","Version":"2.0"}}"#,
)];
const ARCHIVE_B: &[(&str, &str)] = &[(
    "org.example.serialcomicb/metadata.json",
    r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.serialcomicb","Name":"Serial Comic B","Version":"2.0"}}"#,
)];
const ARCHIVE_C: &[(&str, &str)] = &[(
    "org.example.serialcomicc/metadata.json",
    r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.serialcomicc","Name":"Serial Comic C","Version":"2.0"}}"#,
)];
const ARCHIVE_D: &[(&str, &str)] = &[(
    "org.example.serialcomicd/metadata.json",
    r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.serialcomicd","Name":"Serial Comic D","Version":"2.0"}}"#,
)];

/// A fake `kpackagetool6` that installs a comic package like
/// `-t Plasma/Comic -u` does. On each start it appends how many of the four
/// comics' backups exist, and records an overlap whenever it starts while
/// another of its runs still holds the marker directory. The store releases
/// the four downloads together, so installs that are not serialized start
/// together, and the sleep keeps each run open long enough that they meet.
const SERIAL_KPACKAGETOOL: &str = r#"if [ "$1" != "-t" ] || [ "$2" != "Plasma/Comic" ] || [ "$3" != "-u" ]; then
  echo "not a Plasma/Comic package: $*" >&2
  exit 1
fi
find "$XDG_CACHE_HOME/plasmoid-updater/backups" -path '*serialcomic*' -name metadata.json 2>/dev/null \
  | wc -l | tr -d ' ' >> "$XDG_DATA_HOME/kpackagetool6.backups-at-start"
running="$XDG_DATA_HOME/kpackagetool6.running"
if mkdir "$running" 2>/dev/null; then
  held=1
else
  held=0
  echo "$4" >> "$XDG_DATA_HOME/kpackagetool6.overlaps"
fi
sleep 0.3
mkdir -p "$XDG_DATA_HOME/plasma/comics"
cp -a "$4" "$XDG_DATA_HOME/plasma/comics/"
if [ "$held" = 1 ]; then
  rmdir "$running"
fi
exit 0
"#;

fn install_old_comics(sandbox: &Sandbox) {
    let comics = sandbox.data_home().join("plasma/comics");
    let mut entries = String::new();
    for (id, name, package) in COMICS {
        let dir = comics.join(package);
        std::fs::create_dir_all(dir.join("contents/ui")).unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            format!(
                r#"{{"KPackageStructure":"Plasma/Comic","KPlugin":{{"Id":"{package}","Name":"{name}","Version":"1.0"}}}}"#
            ),
        )
        .unwrap();
        write!(
            entries,
            r#"  <stuff category="{COMIC_CATEGORY}">
    <name>{name}</name>
    <providerid>api.kde-look.org</providerid>
    <version>1.0</version>
    <installedfile>{}/metadata.json</installedfile>
    <id>{id}</id>
    <status>installed</status>
  </stuff>
"#,
            dir.display()
        )
        .unwrap();
    }
    let registry = sandbox.data_home().join("knewstuff3/comic.knsregistry");
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    std::fs::write(
        registry,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE khotnewstuff3>\n<hotnewstuffregistry>\n{entries}</hotnewstuffregistry>\n"
        ),
    )
    .unwrap();
}

#[test]
fn same_type_updates_install_one_at_a_time_and_keep_every_registry_entry() {
    if let Some(child) = common::child() {
        // Four workers, so an unserialized pool would run all four installs
        // at once on any runner, whatever its core count.
        let config = Config::new()
            .with_api_url(&child.store_url)
            .with_skip_plasma_detection(true)
            .with_inhibit_idle(false)
            .with_threads(4);
        let result = update(&config, &Quiet).unwrap();
        assert!(result.failed.is_empty(), "{result:?}");
        assert_eq!(result.succeeded.len(), COMICS.len(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comics(&sandbox);
    sandbox.fake_tool("kpackagetool6", SERIAL_KPACKAGETOOL);
    let archives = [ARCHIVE_A, ARCHIVE_B, ARCHIVE_C, ARCHIVE_D];
    let store = FakeStore::start(Listing {
        extras: COMICS
            .iter()
            .zip(archives)
            .map(|(&(id, name, _), files)| ExtraEntry {
                id,
                name,
                type_id: COMIC_CATEGORY,
                version: NEW_VERSION,
                files,
            })
            .collect(),
        extras_download_barrier: COMICS.len(),
        ..Listing::default()
    });
    sandbox.run_child(
        "same_type_updates_install_one_at_a_time_and_keep_every_registry_entry",
        store.url(),
    );

    assert!(
        !store.extras_barrier_timed_out(),
        "the four downloads did not all run at once: one waited {EXTRAS_BARRIER_TIMEOUT:?} for the others"
    );
    let backups_at_start =
        std::fs::read_to_string(sandbox.data_home().join("kpackagetool6.backups-at-start"))
            .unwrap();
    assert_eq!(
        backups_at_start.lines().collect::<Vec<_>>(),
        ["1", "2", "3", "4"],
        "each install must start with only its own and the earlier installs' backups taken"
    );
    let overlaps = sandbox.data_home().join("kpackagetool6.overlaps");
    assert!(
        !overlaps.exists(),
        "two installs ran at once; packages that started during another: {}",
        std::fs::read_to_string(&overlaps).unwrap_or_default()
    );
    let registry =
        std::fs::read_to_string(sandbox.data_home().join("knewstuff3/comic.knsregistry")).unwrap();
    for (id, name, _) in COMICS {
        let entry = registry
            .split("</stuff>")
            .find(|entry| entry.contains(&format!("<id>{id}</id>")))
            .unwrap_or_else(|| panic!("{name} lost its registry entry"));
        assert!(
            entry.contains(&format!("<version>{NEW_VERSION}</version>")),
            "{name}'s registry entry was not bumped to {NEW_VERSION}"
        );
    }
}
