FROM rust:1.95.0-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends clang cmake xz-utils && rm -rf /var/lib/apt/lists/*
RUN curl -fL https://ziglang.org/download/0.16.0/zig-aarch64-linux-0.16.0.tar.xz -o /tmp/zig.tar.xz && tar -xJf /tmp/zig.tar.xz -C /opt && ln -s /opt/zig-aarch64-linux-0.16.0/zig /usr/local/bin/zig
WORKDIR /src
COPY . .
RUN cargo build --locked -p herdr-core
RUN cargo test --locked -p herdr-core --lib classic::tests \
    && cargo test --locked -p herdr-core --test classic classic_create
FROM hydra-compat-modern:c6b316f
COPY --from=build /src/target/debug/herdr-core /usr/local/bin/hc
