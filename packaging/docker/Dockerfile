# The tinycd image: the same binary as the release, plus git and the docker
# command line so an agent inside the container can build and run cartons
# through the host's engine (mount /var/run/docker.sock for that).
#
#   docker build -f packaging/docker/Dockerfile -t tinycd .
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM alpine:3.20
RUN apk add --no-cache git docker-cli ca-certificates curl tini
COPY --from=build /src/target/release/tinycd /usr/local/bin/tinycd
ENV TINYCD_HOME=/data
VOLUME ["/data"]
EXPOSE 4000 4443
ENTRYPOINT ["/sbin/tini", "--", "tinycd"]
CMD ["serve", "--no-tui", "--bind", "0.0.0.0:4000"]
