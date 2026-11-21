# Shopping Sessions: Rust pipeline (src/main.rs). The scorer builds this on linux/amd64.
# Static glibc binary in an empty image: nothing to load at container start.
FROM rust:1.85-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY src/ src/
ENV RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && strip target/x86_64-unknown-linux-gnu/release/sessions \
 && cp target/x86_64-unknown-linux-gnu/release/sessions /sessions

FROM scratch
COPY --from=build /sessions /sessions
# Runs as uid 1000 with a read-only root; reads DATA_DIR, writes OUTPUT_DIR only.
CMD ["/sessions"]
