# Third-party material

This register records build inputs and references. It is a working list, not a legal review.

- **libnx**: homebrew test fixtures (when present) link `-lnx`. Upstream: [switchbrew/libnx](https://github.com/switchbrew/libnx) (ISC and others; see its notices).
- **newlib**: part of the devkitA64 toolchain used to build fixtures. Notices are per-file: [newlib](https://sourceware.org/newlib/).
- **deko3d and `uam`**: used by the deko3d fixture only. Versions and notices to be recorded when that fixture is rebuilt.
- **Public references** used to implement behaviour from documentation (no code copied): the Arm Architecture Reference Manual, SPIR-V, libnx headers, the SwitchBrew wiki, and Mesa/Nouveau class headers.
- **Rust crates**: see `Cargo.lock` and `docs/DEPENDENCY_LICENSE_AUDIT.md`. That inventory records declared Cargo licence metadata only.

See `docs/PROVENANCE.md` for fixture-specific checks.
