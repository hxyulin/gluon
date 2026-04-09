# Dependencies

Gluon manages external crate dependencies directly, without relying on Cargo
for resolution. Dependencies are declared in `gluon.rhai` and linked to
individual crates in the build model.

## Declaring external dependencies

Use the `dependency()` builder to declare an external crate:

```rhai
dependency("log")
    .version("0.4");

dependency("spin")
    .version("0.9")
    .default_features(false)
    .features(["mutex"]);

dependency("my-local-crate")
    .path("../my-local-crate");

dependency("my-git-dep")
    .git("https://github.com/user/repo")
    .branch("main");
```

### Dependency methods

| Method | Description |
|---|---|
| `.version(ver)` | crates.io version requirement |
| `.path(path)` | Local path dependency |
| `.git(url)` | Git repository URL |
| `.branch(name)` | Git branch (used with `.git()`) |
| `.tag(name)` | Git tag (used with `.git()`) |
| `.rev(hash)` | Git revision hash (used with `.git()`) |
| `.default_features(bool)` | Enable or disable default features |
| `.features(list)` | Enable specific features |
| `.optional(bool)` | Mark as an optional dependency |

Source specifiers (`.version()`, `.path()`, `.git()`) are mutually exclusive --
each dependency must use exactly one.

## Linking dependencies to crates

After declaring a dependency at the top level, reference it from a crate with
`.dep()`:

```rhai
dependency("log").version("0.4");

group("kernel")
    .target("x86_64-unknown-none")
    .edition("2021")
    .add("kernel", "crates/kernel")
        .dep("log", "log");
```

The first argument to `.dep()` is the **alias** -- the name used in
`extern crate` or `use` statements in your Rust code. The second argument is
the **dependency name** as declared in the `dependency()` call.

These are often the same, but they can differ when you want to rename a crate:

```rhai
dependency("spin").version("0.9");

// Use as `use spinlock::Mutex;` in Rust code
group("kernel")
    .add("kernel", "crates/kernel")
        .dep("spinlock", "spin");
```

## Vendoring

Gluon can vendor external dependencies into a local `./vendor/` directory for
offline and reproducible builds:

```sh
gluon vendor              # Vendor dependencies, write gluon.lock
gluon vendor --check      # Verify vendor tree integrity without modifying
gluon vendor --force      # Re-vendor even if fingerprints match
gluon vendor --offline    # No network access (lockfile must exist)
```

### How vendoring works

1. Gluon generates a scratch `Cargo.toml` from the declared `dependency()`
   entries.
2. It runs `cargo vendor` to download and populate `./vendor/`.
3. It writes `gluon.lock`, which pins every dependency with checksums.
4. On subsequent builds, Gluon automatically registers vendored crates into the
   build model and compiles them with `rustc` alongside your project crates.

### Version control

The `gluon.lock` file should be committed to version control. This ensures that
every clone of the repository builds against the exact same dependency versions,
regardless of what is currently published on crates.io.

The `vendor/` directory itself can optionally be committed for fully offline
builds, or left out of version control and regenerated with `gluon vendor` as
needed.
