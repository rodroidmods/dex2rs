# syntax=docker/dockerfile:1
#
# The runner image for the cloud converter. Everything a conversion needs is
# baked in, so a workflow run is a container pull and a compile — no
# `cargo install`, no NDK download, no JDK setup.
#
#   docker build -f docker/builder.Dockerfile -t ghcr.io/rodroidmods/dex2rs-builder:1 .
#
# Pin by digest in the workflow once the image is published; the tag is only
# there to make local builds readable.

FROM rust:1.99-bookworm

# The NDK the CLI's own docs target. `cargo ndk` finds it through these two.
ARG NDK_RELEASE=r27c
ARG NDK_VERSION=27.3.13750724
ARG CARGO_NDK_VERSION=4.1.2
ARG GH_VERSION=2.103.3
ARG DEX2RS_VERSION=0.1.1

ENV DEBIAN_FRONTEND=noninteractive \
    ANDROID_NDK_HOME=/opt/android-ndk \
    ANDROID_NDK_ROOT=/opt/android-ndk

# A JRE is not optional: the CLI signs every output by shelling out to
# `java -jar apksigner.jar`, and there is no flag to skip it.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl unzip zip openjdk-17-jre-headless git \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL "https://dl.google.com/android/repository/android-ndk-${NDK_RELEASE}-linux.zip" \
        -o /tmp/ndk.zip \
    && unzip -q /tmp/ndk.zip -d /opt \
    && mv "/opt/android-ndk-${NDK_RELEASE}" /opt/android-ndk \
    && rm /tmp/ndk.zip \
    && echo "$NDK_VERSION" > /opt/android-ndk/source.properties.version

# gh is used by the workflow for the input release and the artifact upload.
RUN curl -fsSL "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.tar.gz" \
      | tar -xz -C /usr/local --strip-components=1 --wildcards '*/bin/gh' \
    && gh --version

RUN cargo install cargo-ndk --locked --version "$CARGO_NDK_VERSION" \
    && cargo install dex2rs --locked --version "$DEX2RS_VERSION" \
    && dex2rs --version

# Every ABI the CLI knows how to build. The workflow passes one `--abis`, but
# having all four here means a run never fails on a missing std.
RUN rustup target add \
        aarch64-linux-android \
        armv7-linux-androideabi \
        i686-linux-android \
        x86_64-linux-android

WORKDIR /work
