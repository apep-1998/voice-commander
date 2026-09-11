//! Reading configuration off disk: which files are picked up, and in what order.

use std::fs;
use vc_core::config::Config;

#[test]
fn a_missing_config_directory_still_yields_the_baseline() {
    let dir = tempfile::tempdir().expect("temp dir");
    let loaded = Config::load_from_dir(&dir.path().join("does-not-exist"))
        .expect("an absent config is not an error — the defaults are complete on their own");
    assert!(loaded.config.profiles.contains_key("default"));
}

#[test]
fn drop_ins_apply_in_filename_order_over_the_main_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conf_d = dir.path().join("conf.d");
    fs::create_dir_all(&conf_d).expect("create conf.d");

    fs::write(
        dir.path().join("config.toml"),
        "[defaults.session]\ncooldown_ms = 1000\n",
    )
    .expect("write config.toml");
    fs::write(
        conf_d.join("10-first.toml"),
        "[defaults.session]\ncooldown_ms = 2000\n",
    )
    .expect("write drop-in");
    fs::write(
        conf_d.join("20-second.toml"),
        "[defaults.session]\ncooldown_ms = 3000\n",
    )
    .expect("write drop-in");

    let loaded = Config::load_from_dir(dir.path()).expect("should load");
    // Numeric prefixes are the convention precisely because the order is lexicographic.
    assert_eq!(loaded.config.defaults.session.cooldown_ms, 3_000);
}

#[test]
fn non_toml_files_in_conf_d_are_ignored() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conf_d = dir.path().join("conf.d");
    fs::create_dir_all(&conf_d).expect("create conf.d");

    // Editor backups and disabled drop-ins live here too and must not break startup.
    fs::write(
        conf_d.join("10-real.toml"),
        "[defaults.session]\ncooldown_ms = 2000\n",
    )
    .expect("write drop-in");
    fs::write(
        conf_d.join("10-real.toml.bak"),
        "this is not toml at all {{{",
    )
    .expect("write backup");
    fs::write(conf_d.join("notes.md"), "# scratch").expect("write note");

    let loaded = Config::load_from_dir(dir.path()).expect("should load");
    assert_eq!(loaded.config.defaults.session.cooldown_ms, 2_000);
}

#[test]
fn a_broken_drop_in_names_its_own_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let conf_d = dir.path().join("conf.d");
    fs::create_dir_all(&conf_d).expect("create conf.d");
    fs::write(conf_d.join("30-broken.toml"), "cooldown_ms = \n").expect("write drop-in");

    let error = Config::load_from_dir(dir.path()).expect_err("should fail");
    assert!(
        error.to_string().contains("30-broken.toml"),
        "must point at the offending file: {error}"
    );
}
