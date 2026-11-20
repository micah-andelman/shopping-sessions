# Shopping Sessions: Rust pipeline (src/main.rs). The scorer builds this on linux/amd64.
FROM rust:1.85-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked && strip target/release/sessions

FROM debian:bookworm-slim
COPY --from=build /src/target/release/sessions /sessions
# Runs as uid 1000 with a read-only root; reads DATA_DIR, writes OUTPUT_DIR only.
CMD ["/sessions"]
