# LDK Node Release Process (Draft)

This document describes a **draft release process** for `ldk-node`.  
It is based on the current repo setup and can be refined by maintainers.

---

## 1. Versioning
- Follow [Semantic Versioning](https://semver.org/) (MAJOR.MINOR.PATCH).
- Update the version number in `Cargo.toml` and make sure it matches the release tag.

---

## 2. Changelog
- Update `CHANGELOG.md` with all user-facing changes:
  - New features
  - Bug fixes
  - Breaking changes
- Use consistent formatting:
  - `### Added`
  - `### Fixed`
  - `### Changed`

---

## 3. Tagging
- Create a signed Git tag for the release:
  ```bash
  git tag -s vX.Y.Z -m "ldk-node vX.Y.Z"
  git push origin vX.Y.Z
