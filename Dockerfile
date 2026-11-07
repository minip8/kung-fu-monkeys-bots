# The bot build image (docs/build.md): the pinned toolchain the ladder builds every submission
# with, and the image tools/build_in_sandbox.sh runs. It holds the emsdk clang, wabt's
# wasm-objdump for the module checks, this repository's sdk/ and tools/build.sh, and an
# unprivileged user `builder`; it needs no network once built.
#   docker build -t kfm-bot-build .
#   tools/build_in_sandbox.sh examples/chaser out/chaser
# The emsdk image is pinned by tag and by digest (its multi-arch index): 6.0.13 ships clang
# 24.0.0git (llvm-project 83ee07799c341b60965f07892f7bb3d175da05be), the compiler the engine's
# example modules were built with. Bumping it changes modules byte for byte: see docs/build.md.
FROM emscripten/emsdk:6.0.13@sha256:fb7864e398acfda863dfcea78b8bc8adad359fa60c6a0bbeb845931de64aa68a

# wabt from its pinned release, checked against the release's own sha256.
ARG WABT_VERSION=1.0.42
ARG TARGETARCH
RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) arch=x64;   sum=84895407a6bbb80e918f33b16b2fb2206021c150b6bc9ff6f761263a745ab131 ;; \
      arm64) arch=arm64; sum=a7632c1cc66f70886441842ecf5a2ffa91a98c79e026059458451234ad013ea8 ;; \
      *) echo "no wabt release for ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    tgz="wabt-${WABT_VERSION}-linux-${arch}.tar.gz"; \
    curl -fsSL -o "/tmp/${tgz}" "https://github.com/WebAssembly/wabt/releases/download/${WABT_VERSION}/${tgz}"; \
    echo "${sum}  /tmp/${tgz}" | sha256sum -c -; \
    tar -xzf "/tmp/${tgz}" -C /opt; \
    ln -s "/opt/wabt-${WABT_VERSION}/bin/wasm-objdump" /usr/local/bin/wasm-objdump; \
    rm "/tmp/${tgz}"; \
    wasm-objdump --version

# The unprivileged build user (a fixed uid, no login shell, no home to write to).
RUN useradd --system --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin builder

# The recipe and the SDK it compiles against: a submission's own copy of sdk/ is never used.
COPY sdk/ /opt/kfm/sdk/
COPY tools/build.sh /opt/kfm/tools/build.sh
RUN printf '#!/bin/sh\nexec /opt/kfm/tools/build.sh /src /out/bot.wasm\n' > /usr/local/bin/kfm-build && \
    chmod 0755 /usr/local/bin/kfm-build /opt/kfm/tools/build.sh && \
    chmod -R a+rX,go-w /opt/kfm

# The compiler by absolute path, so the recipe never depends on emsdk_env.sh or on PATH.
ENV CLANG=/emsdk/upstream/bin/clang
USER builder
WORKDIR /tmp
# No entrypoint: the sandbox command of architecture §8 names its command after the image
# (`IMAGE timeout 120 kfm-build`); a bare `docker run IMAGE` runs the same thing.
ENTRYPOINT []
CMD ["timeout", "120", "kfm-build"]
