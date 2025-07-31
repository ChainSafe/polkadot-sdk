# This is the build stage for Polkadot. Here we create the binary in a temporary image.
FROM docker.io/paritytech/ci-unified:bullseye-1.88.0-2025-06-27-v202507112050 as builder

ARG TARGET_REPO=https://github.com/ChainSafe/polkadot-sdk.git
ARG TARGET_BRANCH=master
ARG CARGO_BUILD_CMD="cargo build --locked --release -p polkadot"

WORKDIR /polkadot

# Clone the ChainSafe polkadot-sdk repo (shallow) and checkout a specific branch, check success
RUN git clone --depth 1 --branch ${TARGET_BRANCH} ${TARGET_REPO} /polkadot && \
    cd /polkadot && \
    git rev-parse --abbrev-ref HEAD | grep -q "^${TARGET_BRANCH}$"

RUN bash -c "${CARGO_BUILD_CMD}"

# This is the 2nd stage: a very small image where we copy the Polkadot binary."
FROM docker.io/paritytech/base-bin:latest

LABEL description="Multistage Docker image for Polkadot: a platform for web3" \
    io.parity.image.type="builder" \
    io.parity.image.authors="devops@chainsafe.io" \
    io.parity.image.vendor="Parity Technologies" \
    io.parity.image.description="Polkadot: a platform for web3" \
    io.parity.image.documentation="https://github.com/paritytech/polkadot-sdk/"

COPY --from=builder /polkadot/target/release/polkadot /usr/local/bin

USER root
RUN useradd -m -u 1001 -U -s /bin/sh -d /polkadot polkadot && \
    mkdir -p /data /polkadot/.local/share && \
    chown -R polkadot:polkadot /data && \
    ln -s /data /polkadot/.local/share/polkadot && \
# unclutter and minimize the attack surface
    # rm -rf /usr/bin /usr/sbin && \
# check if executable works in this container
    /usr/local/bin/polkadot --version

USER polkadot

EXPOSE 30333 9933 9944 9615
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/polkadot"]
