# syntax=docker/dockerfile:1
# CI toolchain for the Linux legs of .github/workflows/ci.yml.
# The official `rust` images ship rustc/cargo only — clippy (the step that is failing on CI) has
# to be added here. `docker/linux-ci.sh` builds one image per architecture from this file.
ARG RUST=1.98.1
FROM rust:${RUST}-bookworm
RUN rustup component add clippy
