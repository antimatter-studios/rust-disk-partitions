#!/usr/bin/env bash
# A release publishes, attests and attaches a tarball for every platform the
# tool ships on -- and every pull request builds each of them first.
#
# v0.1.0 shipped rust-blk-probe-0.1.0-darwin-arm64.tar.gz and nothing else
# (#45), so blk.probe could not be installed on Linux at all, and a tap
# formula had no Linux asset to point at. Nothing noticed: release.yml runs
# only on a tag, and a release with one tarball is exactly as green as one
# with two. This makes the missing platform loud on the pull request that
# drops it.
#
# What is held, parsed as YAML rather than scanned by line:
#
#   release.yml  a job runs scripts/package.sh once for each of PLATFORMS,
#                each on a runner that builds that platform natively; the
#                legs hand their tarballs on with actions/upload-artifact;
#                one job that `needs:` them downloads them, attests the
#                *.tar.gz with actions/attest-build-provenance, and only
#                then attaches the tarballs AND their .sha256 files with
#                `gh release upload`.
#   ci.yml       its packaging job runs scripts/package.sh for each of
#                PLATFORMS on a native runner, so the first build of a
#                platform's tarball is never the tag's.
#
# It proves it fails first, against workflows with a platform dropped, a
# platform built on the wrong runner, and each attestation step removed.
#
#   bash tests/scripts/test-release-platforms.sh
set -uo pipefail

# The workspace root: the workflows are the repository's, not the crate's (#150).
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"

fails=0
ok()   { echo "ok    $*"; }
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

command -v python3 >/dev/null 2>&1 || { echo "FAIL  python3 is required" >&2; exit 1; }
python3 -c 'import yaml' 2>/dev/null ||
    { echo "FAIL  the python3 yaml module is required (pip install pyyaml)" >&2; exit 1; }

# The platforms a release ships. check-package.sh knows how to recognise
# each one's binary; this is the list a release must actually contain.
PLATFORMS="darwin-arm64 linux-x86_64"

# scan <release|ci> <workflow>: every gap, one per line; nothing when clean.
scan() {
    python3 - "$1" "$2" "$PLATFORMS" <<'PY'
import itertools, re, shlex, sys, yaml

kind, path, platforms = sys.argv[1], sys.argv[2], sys.argv[3].split()
doc = yaml.safe_load(open(path)) or {}
jobs = doc.get("jobs") or {}
gaps = []

ATTEST = "actions/attest-build-provenance@"
UPLOAD = "actions/upload-artifact@"
DOWNLOAD = "actions/download-artifact@"

def native(platform, runner):
    """Whether `runner` builds `platform` without cross-compiling."""
    r = str(runner)
    if platform == "darwin-arm64":
        # macos-13 and every *-intel / *-large image is x86_64.
        return r.startswith("macos-") and not re.search(r"^macos-13|intel|large", r)
    if platform == "linux-x86_64":
        return r.startswith("ubuntu-") and not r.endswith("-arm")
    return False

def legs(job):
    """The matrix legs of a job, each a dict of matrix values."""
    matrix = (job.get("strategy") or {}).get("matrix")
    if not isinstance(matrix, dict):
        return [{}]
    axes = {k: v for k, v in matrix.items()
            if k not in ("include", "exclude") and isinstance(v, list)}
    out = [dict(zip(axes, combo)) for combo in itertools.product(*axes.values())] if axes else []
    out += [dict(i) for i in matrix.get("include") or []]
    return out or [{}]

def expand(text, leg):
    return re.sub(r"\$\{\{\s*matrix\.([A-Za-z0-9_-]+)\s*\}\}",
                  lambda m: str(leg.get(m.group(1), m.group(0))), str(text))

def commands(step):
    run = step.get("run") or ""
    return [l.strip() for l in run.replace("\\\n", " ").splitlines()
            if l.strip() and not l.strip().startswith("#")]

def uses(step, prefix):
    return str(step.get("uses") or "").startswith(prefix)

def packaged(job, leg):
    """The platform argument of every scripts/package.sh this leg runs."""
    found = []
    for step in job.get("steps") or []:
        for c in commands(step):
            m = re.search(r"scripts/package\.sh\s+(.*)", expand(c, leg))
            if not m:
                continue
            try:
                args = shlex.split(m.group(1).split(")")[0])
            except ValueError:
                args = m.group(1).split()
            if len(args) >= 3:
                found.append(args[2])
    return found

# --- every platform is packaged, natively ---------------------------------
built = {}                                  # platform -> [(job, runner)]
packaging_jobs = []
for name, job in jobs.items():
    for leg in legs(job):
        for platform in packaged(job, leg):
            runner = expand(job.get("runs-on", ""), leg)
            built.setdefault(platform, []).append((name, runner))
            if name not in packaging_jobs:
                packaging_jobs.append(name)
for platform in platforms:
    if platform not in built:
        gaps.append(f"{kind}: no job packages a {platform} tarball")
        continue
    for name, runner in built[platform]:
        if not native(platform, runner):
            gaps.append(f"{kind}: job {name} packages {platform} on {runner}, "
                        f"which does not build {platform} natively")

if kind == "release":
    # --- the legs hand their tarballs on ----------------------------------
    for name in packaging_jobs:
        if not any(uses(s, UPLOAD) for s in jobs[name].get("steps") or []):
            gaps.append(f"release: job {name} packages a tarball and does not "
                        f"upload it as an artifact for the attesting job")

    # --- one job downloads, attests, then attaches -------------------------
    attesting = [n for n, j in jobs.items()
                 if any(uses(s, ATTEST) for s in j.get("steps") or [])]
    attests_tarballs = False
    for name in attesting:
        job = jobs[name]
        steps = job.get("steps") or []
        at = next(i for i, s in enumerate(steps) if uses(s, ATTEST))
        subject = str((steps[at].get("with") or {}).get("subject-path", ""))
        if ".tar.gz" not in subject:
            continue
        attests_tarballs = True
        needs = job.get("needs") or []
        needs = [needs] if isinstance(needs, str) else needs
        for p in packaging_jobs:
            if p != name and p not in needs:
                gaps.append(f"release: job {name} attests the tarballs without "
                            f"needing {p}, which packages them")
        if not any(uses(s, DOWNLOAD) for s in steps[:at]):
            gaps.append(f"release: job {name} attests tarballs it did not "
                        f"download from the packaging legs")
        attached = [c for s in steps[at + 1:] for c in commands(s)
                    if c.startswith("gh release upload")]
        if not any(".tar.gz" in c for c in attached):
            gaps.append(f"release: job {name} does not attach the attested "
                        f"tarballs to the GitHub release")
        if not any(".sha256" in c for c in attached):
            gaps.append(f"release: job {name} does not attach the tarballs' "
                        f".sha256 files to the GitHub release")
    if not attests_tarballs:
        gaps.append(f"release: no job attests the tarballs with {ATTEST}<sha> "
                    f"(subject-path *.tar.gz)")

for g in gaps:
    print(g)
PY
}

SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/release-platforms.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

# --- 1. The scan refuses what it exists to refuse. ------------------------
SHA=0123456789abcdef0123456789abcdef01234567
cat >"$SANDBOX/good.yml" <<EOF
on:
  push:
    tags: ['v*.*.*']
permissions:
  contents: read
jobs:
  package:
    strategy:
      matrix:
        include:
          - platform: darwin-arm64
            runner: macos-latest
          - platform: linux-x86_64
            runner: ubuntu-latest
    runs-on: \${{ matrix.runner }}
    steps:
      - run: |
          tarball="\$(bash scripts/package.sh target/release/blk_probe "\${GITHUB_REF_NAME#v}" \${{ matrix.platform }} dist)"
      - uses: actions/upload-artifact@$SHA # v4.6.2
  publish:
    needs: [package]
    permissions:
      contents: write
      id-token: write
      attestations: write
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@$SHA # v4.3.0
      - uses: actions/attest-build-provenance@$SHA # v4.2.2
        with:
          subject-path: dist/*.tar.gz
      - run: gh release upload "\$GITHUB_REF_NAME" dist/*.tar.gz dist/*.tar.gz.sha256 --clobber
EOF

found="$(scan release "$SANDBOX/good.yml")" || fail "the scan itself failed"
if [[ -z "$found" ]]; then
    ok "the scan accepts a release that packages, attests and attaches every platform"
else
    fail "the scan refused a good release:"$'\n'"$found"
fi

# refused <description> <expected gap> <sed expression>
refused() {
    sed -e "$3" "$SANDBOX/good.yml" >"$SANDBOX/bad.yml"
    if cmp -s "$SANDBOX/good.yml" "$SANDBOX/bad.yml"; then
        fail "the mutation for '$1' changed nothing"
        return
    fi
    found="$(scan release "$SANDBOX/bad.yml")" || { fail "the scan itself failed on '$1'"; return; }
    if grep -qF "$2" <<<"$found"; then
        ok "the scan refuses $1"
    else
        fail "the scan missed $1: expected '$2', got:"$'\n'"${found:-<nothing>}"
    fi
}

refused "a release with no linux-x86_64 leg" \
    "no job packages a linux-x86_64 tarball" \
    '/platform: linux-x86_64/,/runner: ubuntu-latest/d'
refused "a release with no darwin-arm64 leg" \
    "no job packages a darwin-arm64 tarball" \
    '/platform: darwin-arm64/,/runner: macos-latest/d'
refused "linux-x86_64 built on an arm runner" \
    "packages linux-x86_64 on ubuntu-24.04-arm" \
    's/runner: ubuntu-latest/runner: ubuntu-24.04-arm/'
refused "darwin-arm64 built on an Intel runner" \
    "packages darwin-arm64 on macos-13" \
    's/runner: macos-latest/runner: macos-13/'
refused "legs that do not hand their tarballs on" \
    "does not upload it as an artifact" \
    '/actions\/upload-artifact/d'
refused "an attesting job that does not download the legs' tarballs" \
    "attests tarballs it did not download" \
    '/actions\/download-artifact/d'
refused "an attesting job that does not wait for the legs" \
    "without needing package" \
    's/needs: \[package\]/needs: []/'
refused "a release that attests nothing" \
    "no job attests the tarballs" \
    's|subject-path: dist/\*.tar.gz|subject-path: Cargo.toml|'
refused "a release that does not attach the tarballs" \
    "does not attach the attested tarballs" \
    's/gh release upload/echo gh-release-upload/'
refused "a release that attaches the tarballs without their checksums" \
    "does not attach the tarballs' .sha256" \
    's| dist/\*.tar.gz.sha256||'

# --- 2. The real workflows. -----------------------------------------------
for pair in "release:.github/workflows/release-blk-probe.yml" "ci:.github/workflows/ci.yml"; do
    kind="${pair%%:*}"
    wf="$REPO/${pair#*:}"
    if [[ ! -f "$wf" ]]; then
        fail "${pair#*:} is missing"
        continue
    fi
    found="$(scan "$kind" "$wf")" || { fail "the scan itself failed on ${pair#*:}"; continue; }
    if [[ -z "$found" ]]; then
        ok "${pair#*:} packages every platform: $PLATFORMS"
    else
        fail "${pair#*:} does not ship every platform:"$'\n'"$found"
    fi
done

if (( fails > 0 )); then
    echo "FAIL  $fails check(s) failed" >&2
    exit 1
fi
echo "PASS  a release packages, attests and attaches a tarball for each of: $PLATFORMS"
