# AeroSync Python SDK — Release Checklist

This document walks the human release operator through the manual
steps that the CI cannot perform on its own. The CI workflow at
`.github/workflows/python-release.yml` builds wheels + sdist and
publishes via PyPI Trusted Publisher (OIDC); everything below is the
*one-time* setup that must be done **before** the workflow can
publish for the first time, plus the *per-release* dance.

If you are picking this up cold, also read
[`docs/rfcs/RFC-001-python-sdk.md`](../rfcs/RFC-001-python-sdk.md)
§8 (Build & distribution).

---

## One-time setup (manual, by repo admin)

These steps were intentionally NOT automated by the w7 subagent — they
require a human with admin access to the PyPI account, the TestPyPI
account, and the GitHub repository settings.

### 1 · Reserve the `aerosync` name

- [ ] **PyPI**: log in at <https://pypi.org/>, then visit
      <https://pypi.org/manage/projects/>. If "aerosync" is unclaimed,
      either:
  - Upload a 0.0.0 placeholder via `twine upload` (legacy path), OR
  - Use the "Add a new pending publisher" flow below (preferred —
    it reserves the name AND configures Trusted Publisher in one
    step).
- [ ] **TestPyPI**: same workflow, on <https://test.pypi.org/>.
      Reserving on TestPyPI separately is required because it is a
      completely independent index.

### 2 · Configure PyPI Trusted Publisher

- [ ] Visit <https://pypi.org/manage/account/publishing/>.
- [ ] Click **"Add a new pending publisher"**.
- [ ] Fill in:

  | Field                      | Value                                |
  | -------------------------- | ------------------------------------ |
  | PyPI project name          | `aerosync`                           |
  | Owner                      | `TechVerseOdyssey`                   |
  | Repository name            | `AeroSync`                           |
  | Workflow filename          | `python-release.yml`                 |
  | Environment name           | `pypi`                               |

- [ ] Save. PyPI will mark the publisher as "pending" until the first
      successful upload from the matching workflow + environment, at
      which point the project is created and the publisher becomes
      active. **The first publish must happen via this workflow** —
      if you upload via `twine` first, the OIDC binding will need
      another setup pass.

### 3 · Configure TestPyPI Trusted Publisher

- [ ] Visit <https://test.pypi.org/manage/account/publishing/> and
      repeat the form, with Environment `testpypi`. Same workflow
      filename.

### 4 · Create the GitHub Environments

- [ ] Repo Settings → Environments → **New environment**.
- [ ] Create `pypi` and `testpypi`.
- [ ] (Recommended for `pypi` only) under "Required reviewers" add
      yourself or a release-team member. This forces a manual approval
      click in the GitHub UI before the production publish job
      actually runs, even if a tag is pushed.
- [ ] No deployment secrets are needed — Trusted Publisher uses OIDC,
      so neither environment should have any `secrets` configured.

### 5 · Sanity-check the workflow file

- [ ] Confirm `.github/workflows/python-release.yml` exists on the
      default branch.
- [ ] Confirm `permissions: id-token: write` is set at the workflow
      level (it is, in the file shipped with this checklist).
- [ ] (Optional) push to a topic branch and trigger
      `workflow_dispatch` with `target=testpypi`. This builds the
      full wheel matrix without publishing if you want to validate
      the matrix end-to-end before the first tag.

---

## Per-release dance

Run through this list every time you cut a new SDK release.

### 1 · Bump versions (in lockstep)

- [ ] `aerosync-py/Cargo.toml` — `[package].version`
- [ ] `aerosync-py/pyproject.toml` — `[project].version`
- [ ] Both must match semantically. The Rust crate isn't published,
      but maturin uses `Cargo.toml` as the source of truth for the
      wheel filename's version segment. Note that pre-release suffixes
      differ in spelling between the two files — Cargo uses SemVer
      (`0.3.0-rc1`) while `pyproject.toml` uses PEP 440 normalized
      form (`0.3.0rc1`, no hyphen). Stable releases (`0.3.0`) are
      identical in both.
- [ ] No other files need a version bump. In particular,
      `aerosync-py/tests/test_smoke.py` reads `[package].version`
      from `Cargo.toml` at test-time, so the version assertion stays
      in sync automatically — there is no constant to edit.

### 2 · Update CHANGELOG.md

- [ ] Move `## Unreleased` entries under a new `## [<version>] —
      <date>` heading.
- [ ] Add the GitHub compare link at the bottom of the file.

### 3 · TestPyPI dry run (mandatory)

- [ ] Push the version-bump commit to `master` (or a release branch
      whose `python-release.yml` is identical).
- [ ] In the GitHub UI: Actions → **Python Release** → Run workflow
      → set `target=testpypi`.
- [ ] Wait for the `wheels-*` matrix + `sdist` jobs to finish.
- [ ] Wait for `publish → TestPyPI` to finish.
- [ ] Verify install works from a fresh venv:

  ```bash
  python -m venv /tmp/aerosync-testpypi
  source /tmp/aerosync-testpypi/bin/activate
  pip install --index-url https://test.pypi.org/simple/ \
              --extra-index-url https://pypi.org/simple/ \
              "aerosync==<version>"
  python -c "import aerosync; print(aerosync.version())"
  ```

- [ ] The version printed must match the bump.

### 4 · Production release

- [ ] Tag the commit:

  ```bash
  git tag -a v<version> -m "AeroSync <version>"
  git push origin v<version>
  ```

- [ ] The tag push triggers `python-release.yml` (build + sdist
      jobs). It does NOT publish on its own — production publishing
      is gated on the GitHub Release being **published**, OR a manual
      `workflow_dispatch` with `target=pypi`.
- [ ] Create the GitHub Release from the tag (Releases → Draft a new
      release → pick the tag → "Generate release notes" → Publish).
      The publish action then triggers the `publish → PyPI` job.
- [ ] If you set required reviewers on the `pypi` environment in
      step 4 of one-time setup, you'll get a "review pending"
      notification — approve it.
- [ ] Watch the `publish → PyPI` job. On success, the wheels +
      sdist are live at <https://pypi.org/project/aerosync/>.

### 5 · Post-release verification

- [ ] Fresh venv install from real PyPI:

  ```bash
  python -m venv /tmp/aerosync-pypi
  source /tmp/aerosync-pypi/bin/activate
  pip install "aerosync==<version>"
  python -c "import aerosync; print(aerosync.version())"
  ```

- [ ] Optionally run a one-shot send/receive smoke between two hosts
      to confirm the wheel actually links against the engine.

---

## Notes / known caveats

- **Windows-on-ARM** is NOT in the wheel matrix — see comment inline
  in `python-release.yml`. Tracked for evaluation in a future minor
  release once GitHub Actions / cibuildwheel runners stabilise on
  that target.
- **musllinux aarch64** likewise deferred to a future minor release;
  needs more soak time on the maturin-action qemu path before we
  ship it as a supported wheel.
- **abi3 tagging** is single-wheel-per-platform: each wheel works on
  CPython 3.9, 3.10, 3.11, 3.12, … Bumping the abi3 floor is a SemVer
  break for the SDK and must be a major-version bump.
- **No tokens in GitHub Secrets.** If you find yourself reaching for
  `PYPI_API_TOKEN` or `TWINE_PASSWORD`, stop — Trusted Publisher
  obviates both. The token would also need to be rotated forever, and
  a leak is a much bigger blast radius than the OIDC-scoped flow.
