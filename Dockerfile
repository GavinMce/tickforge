# The tickforge command line as a container image: what the dev environment runs (`tf serve`, `tf live`, `tf research`).
# Built in CI (.github/workflows/image.yml) and published to ghcr.io/gavinmce/tickforge.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p tf-cli && strip target/release/tf

FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/GavinMce/tickforge" \
      org.opencontainers.image.description="tickforge command line" \
      org.opencontainers.image.licenses="UNLICENSED"
# curl and certificates for the scripts that pull from Databento; tzdata for the New York dates the scripts compute.
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/*
# A fixed unprivileged user, so a mounted volume can be given to it by number.
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin tf
COPY --from=build /src/target/release/tf /usr/local/bin/tf
COPY scripts/ /opt/tickforge/scripts/
ENV PATH="/opt/tickforge/scripts:/usr/local/bin:/usr/bin:/bin"
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/tf"]
