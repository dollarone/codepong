# 1.52.1, probably. slim-buster
FROM rust@sha256:14f30a50809805fec8b6dd4ad31ebb68f2a71e12ffd324b50ec8d8992449b374 as build

WORKDIR /src
ENV USER root

RUN cargo new --bin codepong \
&& apt-get update \
&& apt-get install -y pkg-config libssl-dev

WORKDIR /src/codepong

COPY ./Cargo.lock ./
COPY ./Cargo.toml ./

# Cache deps
RUN \
cargo build --release \
&& rm src/*.rs

# Copy real source code
COPY ./src/ ./src
COPY ./handlebars/ ./handlebars
COPY ./static/ ./static

# For good luck, copied from PTTH
RUN \
touch src/main.rs \
&& cargo build --release \
&& cargo test --release

# debian:buster-slim
FROM debian@sha256:f077cd32bfea6c4fa8ddeea05c53b27e90c7fad097e2011c9f5f11a8668f8db4

RUN \
apt-get update \
&& apt-get upgrade -y \
&& apt-get install -y libssl1.1 ca-certificates tini \
&& addgroup --gid 10001 user \
&& adduser --system --uid 10000 --gid 10001 user

USER user
WORKDIR /home/user

COPY --from=build /src/codepong/target/release/codepong ./
COPY --from=build /src/codepong/handlebars ./handlebars
COPY --from=build /src/codepong/static ./static

ARG git_version
RUN \
echo -n "$git_version" > ./git_version.txt && \
ln -s codepong app

CMD ["/usr/bin/tini", "--", "./codepong"]
