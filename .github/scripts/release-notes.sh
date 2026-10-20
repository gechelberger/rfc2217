#!/usr/bin/env bash
# Prints the release notes for VERSION: the body of the
# "## [VERSION] - YYYY-MM-DD" section of a Keep a Changelog file, without the
# heading. The body runs to the next "## " heading. Link reference definitions
# inside it are kept; only the block of them that closes the file is left out
# (that matters for the last section in the file).
# Fails (exit 1, with a GitHub Actions ::error:: annotation) when:
#   - there is not exactly one "## [VERSION]" heading,
#   - the heading is not exactly "## [VERSION] - YYYY-MM-DD" with a real date,
#   - the date is later than today (UTC) + 1 day, or earlier than the commit
#     date of COMMIT (UTC) - 1 day (a date left over from an earlier draft),
#   - the section body is empty,
#   - the "[VERSION]: <url>" link reference is missing, or its URL does not
#     end in "/TAG" (a releases/tag URL) or ".TAG" (a compare/a...b URL).
#     Requiring the separator keeps a changelog that links the other crate's
#     tags from passing: "rfc2217-cli-v0.1.0" ends in "v0.1.0" too.
#
# Usage: bash .github/scripts/release-notes.sh VERSION [CHANGELOG] [COMMIT] [TAG]
#   CHANGELOG defaults to CHANGELOG.md, COMMIT to HEAD, TAG to vVERSION (the
#   library's tags; release-cli.yml passes rfc2217-cli-vVERSION).
# release.yml and release-cli.yml run it before publishing; run it locally
# before tagging to see the notes the GitHub release will get.
# Matching is literal (no regex built from VERSION or TAG), so "1.2.3" never
# matches "1x2y3" and pre-release versions such as "1.0.0-rc.1" work unchanged.
set -euo pipefail

fail() {
    printf '::error::%s\n' "$*" >&2
    exit 1
}

[ $# -ge 1 ] && [ $# -le 4 ] || fail "usage: $0 VERSION [CHANGELOG] [COMMIT] [TAG]"
version=$1
changelog=${2:-CHANGELOG.md}
commit=${3:-HEAD}
tag=${4:-v$version}
[ -f "$changelog" ] || fail "$changelog not found"

prefix="## [$version]"

# Every heading line for this version (literal prefix match, then "" or " ").
headings=$(awk -v p="$prefix" '
    substr($0, 1, length(p)) == p {
        rest = substr($0, length(p) + 1)
        if (rest == "" || substr(rest, 1, 1) == " ") print
    }' "$changelog")
count=$(printf '%s' "$headings" | grep -c '^' || true)
[ "$count" -eq 1 ] || fail "expected one '$prefix - YYYY-MM-DD' heading in $changelog, found $count"

date=${headings#"$prefix - "}
[ "$headings" = "$prefix - $date" ] || fail "heading must be '$prefix - YYYY-MM-DD', found '$headings'"
[[ $date =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || fail "'$date' in '$headings' is not a YYYY-MM-DD date"
# GNU date normalises impossible dates (2026-02-30) to other days; reject those.
[ "$(date -u -d "$date" +%F 2>/dev/null || true)" = "$date" ] || fail "'$date' in '$headings' is not a calendar date"

day=86400
release_s=$(date -u -d "$date" +%s)
today_s=$(date -u -d "$(date -u +%F)" +%s)
commit_day=$(TZ=UTC git log -1 --format=%cd --date=format-local:%F "$commit")
commit_s=$(date -u -d "$commit_day" +%s)
[ "$release_s" -le $((today_s + day)) ] || fail "'$headings' is dated in the future (today is $(date -u +%F) UTC)"
[ "$release_s" -ge $((commit_s - day)) ] ||
    fail "'$headings' is dated before the tagged commit ($commit_day UTC): update the date"

# The link reference, e.g. "[0.2.0]: https://github.com/o/r/compare/v0.1.0...v0.2.0":
# the URL is non-empty and ends in TAG, right after a "/" or a ".".
awk -v p="[$version]: " -v t="$tag" '
    substr($0, 1, length(p)) == p && length($0) > length(p) + length(t) &&
        substr($0, length($0) - length(t) + 1) == t {
        sep = substr($0, length($0) - length(t), 1)
        if (sep == "/" || sep == ".") found = 1
    }
    END { exit !found }' "$changelog" ||
    fail "missing link reference '[$version]: <url ending in /$tag or .$tag>' in $changelog"

# The body: after the heading, up to the next "## " heading. Link reference
# definitions inside the section stay, with whatever follows them. If the
# section runs to the end of the file, the file's closing run of link reference
# definitions and blank lines (the "[Unreleased]: ..." block) is left out.
# Leading and trailing blank lines are removed.
body=$(awk -v h="$headings" '
    { line[NR] = $0 }
    !start && $0 == h { start = NR; next }
    start && !end && /^## / { end = NR }
    END {
        if (!end) {
            end = NR + 1
            while (end - 1 > start &&
                (line[end - 1] !~ /[^[:space:]]/ || line[end - 1] ~ /^\[[^]]+\]: /))
                end--
        }
        for (i = start + 1; i < end; i++) print line[i]
    }' "$changelog" | sed -e '/[^[:space:]]/,$!d')
# $(...) already dropped trailing newlines; drop trailing blank-only lines too.
body=$(printf '%s\n' "$body" | awk '{ lines[NR] = $0 } /[^[:space:]]/ { last = NR } END { for (i = 1; i <= last; i++) print lines[i] }')
[ -n "$body" ] || fail "the '$headings' section is empty"

printf '%s\n' "$body"
