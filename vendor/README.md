# Maintaining vendored dependencies

Inputtino and Pixelforge use the same maintenance workflow. Each source tree is
generated from an immutable upstream commit plus a numbered patch series.
Dependency-specific behavior and hardware acceptance checks live in each
`LOCAL_CHANGES.md`.

| Dependency | Generated source | Patch series and base | Local changes |
| --- | --- | --- | --- |
| Inputtino | `inputtino/` | `inputtino-patches/`, `inputtino-patches/UPSTREAM` | [Inputtino notes](inputtino/LOCAL_CHANGES.md) |
| Pixelforge | `pixelforge/` | `pixelforge-patches/`, `pixelforge-patches/UPSTREAM` | [Pixelforge notes](pixelforge/LOCAL_CHANGES.md) |

## Source and patch format

`UPSTREAM` records `commit <40-character SHA>` and `describe <tag or short SHA>`.
The commit is authoritative; descriptions are human-readable. Patches use
`0001-description.patch`, `0002-description.patch`, and so on, in lexical order,
with paths relative to the upstream repository root. Prefer one concern per
patch and create patches with `git format-patch` in a temporary upstream
checkout. Preserve upstream license notices.

Never edit generated source directly. The two `scripts/vendor-*.sh` entry points
share `scripts/vendor-dependency.sh`: clone, check out the pin, apply each patch,
and copy the dependency's selected files. Regeneration replaces the generated
files only after every patch applies, preserving the root `LOCAL_CHANGES.md`.
`--check` rebuilds in a temporary directory and detects content, symlink,
executable-mode and file-inventory drift without rewriting source or pins.

The root Cargo patches keep the upstream dependency declarations and substitute
only Inputtino's native `inputtino-sys` backend and the Pixelforge crate. The
Inputtino public Rust crate has an explicit `rev` matching its native base;
Pixelforge keeps its upstream release tag, verified against the base commit.
`--check` also checks these declarations, local crate versions and Cargo.lock.
Nix includes both source trees through `nix/package.nix`'s vendor fileset;
Inputtino's existing Nix C++ linker adjustment remains a packaging adaptation.

## Updating

Review upstream changes and obtain owner approval before applying an upstream
update, following [the repository policy](../AGENTS.md#upstream-synchronization).
An update is a reviewed change, never an automatic tracking-branch build.

The daily [Vendor upstream workflow](../.github/workflows/vendor-upstream.yaml)
proposes updates: when Inputtino's default branch or a newer Pixelforge release
tag moves past the pin, it regenerates the tree with the existing patches,
moves the Cargo pins, runs `--check` and opens a `vendor-update/<dependency>`
pull request. If regeneration fails, usually because a patch no longer
applies, it opens an issue instead. Merging that pull request is the approval;
the checklist in its description covers the steps below that it cannot run.
CI runs on these pull requests only when the `VENDOR_UPDATE_TOKEN` secret is set.

```sh
# After editing a patch, regenerate at the existing pin.
scripts/vendor-inputtino.sh
scripts/vendor-pixelforge.sh

# Verify source reconstruction and Cargo pins.
scripts/vendor-inputtino.sh --check
scripts/vendor-pixelforge.sh --check

# Run each dependency's hardware-independent regression suite.
scripts/vendor-inputtino.sh --test
scripts/vendor-pixelforge.sh --test
```

To use an existing upstream Git checkout or mirror, add `--source /path/to/repo`
to regeneration or `--check`. The repository must contain the pinned commit and,
for Pixelforge, its release tag. This supports offline verification; it does not
change the upstream source recorded in Cargo or the pin.

For an approved base update, pass the exact upstream ref to the corresponding
script. Rebase each patch in order, dropping it only when upstream provides the
same behavior. Update the dependency's Cargo `rev` or `tag`, refresh Cargo.lock,
and update its notes. Run both `--check` commands, dependency tests and relevant
workspace, Nix and hardware checks before committing. If no patches remain,
review removal of the vendor override and return to the pinned upstream source.

## Validation

CI verifies reconstruction and Cargo pin consistency for both dependencies and
runs Pixelforge's unit tests explicitly: workspace tests do not run the unit
tests of a path-patched dependency. Inputtino's hardware-independent tests run
under ASan/UBSan in the existing known-defects CI step; `--test` provides the same
native suite directly. Maintenance-tool regression tests run with:

```sh
python3 -m unittest discover -s scripts -p 'test_vendor_dependency.py'
```

Dependency tests do not replace the controller device/TSan or GPU encode/decode
and reconnect acceptance checks documented in the local notes. `CARGO_TARGET_DIR`
can select the build-output directory for either `--test` command.
