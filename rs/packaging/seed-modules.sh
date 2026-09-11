#!/usr/bin/env bash
# Build the in-tree first-party modules and stage them for first-run seeding.
#
# Requirement #2 ("first-party modules installed by default") ships each module as a
# compiled artifact inside the bundle; core::install::seed::seed_bundled walks the staged
# tree on first launch and installs each through the ordinary InstallStore seam — offline,
# no toolchain, no clone. This is the packaging half: it produces the tree that
# core::install::seed::seed_modules_dir() resolves at runtime.
#
# Sourced by the platform packagers (macOS bundle.sh, appimage.sh, deb.sh, rpm.sh) — it
# defines one function and runs nothing on its own — but also runnable standalone for a
# dry check:
#
#   rs/packaging/seed-modules.sh <dest_seed_dir>
#
# Layout produced under <dest_seed_dir> (matches seed.rs's documented contract):
#
#   <dest>/<owner>__<repo>/
#     avada.toml            the module's manifest, verbatim
#     bin/<name>            the release binary (name = distribution.bin or the repo part
#                           of the id; none of the in-tree modules override it)
#     <skills paths...>     each directory named by the manifest's [skills] paths
#
# The modules are members of the LEAN ROOT workspace (rs/Cargo.toml), not of the app
# crate's own workspace, so they build with --manifest-path rs/Cargo.toml and land in that
# workspace's target/release. Errors are fatal here (unlike the runtime, which degrades):
# a packager that cannot stage a module must fail the build, not ship a broken bundle.

# Absolute path to the repo root, from this file's location (rs/packaging → ../..).
# The real packagers run under bash (BASH_SOURCE works); the zsh branch only makes a
# `source rs/packaging/seed-modules.sh` dry-check work from a zsh prompt, where
# BASH_SOURCE is unset and `${(%):-%x}` names the sourced file instead.
if [ -n "${BASH_SOURCE:-}" ]; then _seed_self="${BASH_SOURCE[0]}"; else _seed_self="${(%):-%x}"; fi
_seed_script_dir="$(cd "$(dirname "$_seed_self")" && pwd)"
SEED_ROOT="$(cd "$_seed_script_dir/../.." && pwd)"

# The manifest key under [module]; the FIRST `id =` line (contributions carry their own
# `id =` further down). Prints owner/repo, e.g. bshuler/avada-files.
_seed_module_id() {
    sed -n 's/^id = "\([^"]*\)".*/\1/p' "$1" | head -1
}

# distribution.bin if the [distribution] table sets it, else empty. Kept general even
# though no in-tree module overrides it today.
_seed_bin_override() {
    awk '/^\[distribution\]/{f=1;next} /^\[/{f=0} f && /^bin *=/{
        sub(/^bin *= *"/,""); sub(/".*/,""); print; exit }' "$1"
}

# The quoted items of the [skills] `paths = [ ... ]` array, one per line (empty if none).
_seed_skill_paths() {
    awk '/^\[skills\]/{f=1;next} /^\[/{f=0} f && /paths *=/{
        # everything between the first [ and the last ]
        s=$0; sub(/^[^[]*\[/,"",s); sub(/\].*/,"",s);
        n=split(s, parts, ",");
        for (i=1;i<=n;i++){ gsub(/[" ]/,"",parts[i]); if (parts[i]!="") print parts[i] }
    }' "$1"
}

# stage_seed_modules <dest_seed_dir>
# Build every in-tree module once, then stage each into <dest_seed_dir>.
stage_seed_modules() {
    local dest="$1"
    [ -n "$dest" ] || { echo "stage_seed_modules: missing <dest_seed_dir>" >&2; return 2; }

    local modules_dir="$SEED_ROOT/rs/modules"
    [ -d "$modules_dir" ] || { echo "stage_seed_modules: no $modules_dir" >&2; return 1; }

    # Collect (dir, pkg) pairs and build them all in one cargo invocation so the shared SDK
    # compiles once, not once per module.
    local -a dirs=() pkgs=() build_args=()
    local d pkg
    for d in "$modules_dir"/*/; do
        [ -f "$d/avada.toml" ] || continue
        [ -f "$d/Cargo.toml" ] || continue
        pkg="$(sed -n 's/^name = "\([^"]*\)".*/\1/p' "$d/Cargo.toml" | head -1)"
        [ -n "$pkg" ] || { echo "stage_seed_modules: no package name in $d/Cargo.toml" >&2; return 1; }
        dirs+=("$d"); pkgs+=("$pkg"); build_args+=(-p "$pkg")
    done
    [ "${#dirs[@]}" -gt 0 ] || { echo "stage_seed_modules: no modules under $modules_dir" >&2; return 1; }

    echo "==> building ${#dirs[@]} in-tree modules (release)"
    cargo build --release --manifest-path "$SEED_ROOT/rs/Cargo.toml" "${build_args[@]}" -j 4

    # The root workspace's target dir — CARGO_TARGET_DIR wins, else rs/target.
    local target_dir="${CARGO_TARGET_DIR:-$SEED_ROOT/rs/target}"

    echo "==> staging modules into $dest"
    rm -rf "$dest"
    local i id owner repo binname binsrc reldir p
    for i in "${!dirs[@]}"; do
        d="${dirs[$i]}"; pkg="${pkgs[$i]}"
        id="$(_seed_module_id "$d/avada.toml")"
        [ -n "$id" ] || { echo "stage_seed_modules: no module id in $d/avada.toml" >&2; return 1; }
        owner="${id%%/*}"; repo="${id##*/}"
        binname="$(_seed_bin_override "$d/avada.toml")"; [ -n "$binname" ] || binname="$repo"

        binsrc="$target_dir/release/$binname"
        [ -x "$binsrc" ] || { echo "stage_seed_modules: built binary not found at $binsrc" >&2; return 1; }

        reldir="$dest/${owner}__${repo}"
        mkdir -p "$reldir/bin"
        install -m 755 "$binsrc" "$reldir/bin/$binname"
        install -m 644 "$d/avada.toml" "$reldir/avada.toml"

        # Skills are directories (seed.rs stages them with stage_skills, which requires
        # each declared path to be a directory holding <name>/SKILL.md).
        while IFS= read -r p; do
            [ -n "$p" ] || continue
            [ -d "$d/$p" ] || { echo "stage_seed_modules: [skills] path '$p' is not a directory in $d" >&2; return 1; }
            mkdir -p "$reldir/$(dirname "$p")"
            cp -R "$d/$p" "$reldir/$p"
        done < <(_seed_skill_paths "$d/avada.toml")

        echo "    staged ${owner}__${repo} (bin: $binname)"
    done
}

# Standalone: `seed-modules.sh <dest>` stages, for a dry check outside a packager.
if [ "${BASH_SOURCE[0]}" = "${0}" ]; then
    set -euo pipefail
    stage_seed_modules "${1:?usage: seed-modules.sh <dest_seed_dir>}"
    echo "==> done"
fi
