//! Prints where Keystone finds each program named on the command line, as an app does,
//! one `name<TAB>path` line each, with `-` for none. `scripts/check.sh providers` runs
//! it under a test binary's name, built with the test features, to prove that only test
//! setup can stop an app's search for `claude` or `codex` (TOB-363).

fn main() {
    for name in std::env::args().skip(1) {
        let found = keystone_agent::find::program(&name);
        let path = found.map_or_else(|| "-".to_owned(), |path| path.display().to_string());
        println!("{name}\t{path}");
    }
}
