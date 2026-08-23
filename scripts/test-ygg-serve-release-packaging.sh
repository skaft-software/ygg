#!/usr/bin/env bash
set -euo pipefail

script_directory=$(cd "$(dirname "$0")" && pwd)
source_packager="$script_directory/package-ygg-serve-release.sh"
work_directory=$(mktemp -d "${TMPDIR:-/tmp}/ygg-serve-package-test.XXXXXX")
trap 'rm -rf "$work_directory"' EXIT
repository="$work_directory/repository"
output_directory="$work_directory/output"
target=x86_64-unknown-linux-gnu

mkdir -p "$repository/scripts" "$repository/target/$target/release"
cp "$source_packager" "$repository/scripts/package-ygg-serve-release.sh"
chmod 0755 "$repository/scripts/package-ygg-serve-release.sh"
printf '%s\n' '/target/' >"$repository/.gitignore"
printf '%s\n' 'release fixture' >"$repository/README.md"
cat >"$repository/target/$target/release/ygg" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --version ]]; then
    printf '%s\n' 'ygg 0.4.0'
    exit 0
fi
exit 2
EOF
chmod 0755 "$repository/target/$target/release/ygg"

git -C "$repository" init -q
git -C "$repository" config user.name 'Ygg release test'
git -C "$repository" config user.email 'release-test@example.invalid'
git -C "$repository" add .gitignore README.md scripts/package-ygg-serve-release.sh
git -C "$repository" -c commit.gpgSign=false commit -q -m 'release fixture'

packager="$repository/scripts/package-ygg-serve-release.sh"
"$packager" "$target" "$output_directory" v0.4.0 >"$work_directory/clean.stdout" 2>"$work_directory/clean.stderr"
test -f "$output_directory/ygg-serve-0.4.0-$target.tar.gz"

printf '%s\n' 'dirty' >>"$repository/README.md"
if "$packager" "$target" "$work_directory/tracked-output" v0.4.0 >"$work_directory/tracked.stdout" 2>"$work_directory/tracked.stderr"; then
    printf 'Serve release packaging accepted tracked source changes\n' >&2
    exit 1
fi
grep -F 'release source has tracked changes; package an immutable clean commit' "$work_directory/tracked.stderr" >/dev/null
git -C "$repository" checkout -q -- README.md

mkdir -p "$repository/extensions/ygg-serve/src"
printf '%s\n' '// uncommitted release source' >"$repository/extensions/ygg-serve/src/untracked.rs"
if "$packager" "$target" "$work_directory/untracked-output" v0.4.0 >"$work_directory/untracked.stdout" 2>"$work_directory/untracked.stderr"; then
    printf 'Serve release packaging accepted untracked source files\n' >&2
    exit 1
fi
grep -F 'release source has untracked files; package an immutable clean commit' "$work_directory/untracked.stderr" >/dev/null

echo 'Ygg Serve release packaging accepts only a clean committed source tree'
