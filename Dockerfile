FROM rust:1.98.1-slim-bookworm AS agent-build
WORKDIR /src/agent
COPY agent/Cargo.toml agent/Cargo.lock ./
COPY agent/src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry cargo build --locked --release

FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive
# Don't update bootloader: https://bugs.debian.org/cgi-bin/bugreport.cgi?bug=594189
ENV INITRD=No
ENV LANG=en_US.UTF-8

RUN echo 'force-unsafe-io' >> /etc/dpkg/dpkg.cfg.d/02apt-speedup && \
    apt-get update && \
    apt-get -y install curl && \
    apt-get install -y --no-install-recommends apt-utils && \
    apt-get -y install \
      python3 \
      git-core bash emacs-nox wget \
      build-essential autoconf libtool pkg-config meson ninja-build cmake cmake-curses-gui gperf \
      zlib1g-dev libbz2-dev liblzma-dev \
      libpng-dev libjpeg-dev libtiff-dev libgif-dev librsvg2-dev \
      libde265-dev \
      libssl-dev \
      libexpat1-dev \
      uuid-dev \
      file locales \
    && \
    locale-gen $(bash -c 'echo ${LANG%.*}') ${LANG} && \
    apt-get clean && \
    rm -r /var/lib/apt/lists/*

# x86-only assemblers (yasm, nasm) - not needed on ARM
RUN if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
      apt-get update && apt-get -y install yasm nasm && \
      apt-get clean && rm -r /var/lib/apt/lists/*; \
    fi

# Use bash because I want to use pipefail in this build.
SHELL ["/bin/bash", "-c"]

ARG PREFIX=/usr/local
ARG DEPS_CONFIGURE_OPTS="--prefix=${PREFIX} --enable-static --enable-pic"
ENV PKG_CONFIG_PATH=${PREFIX}/lib/pkgconfig
# Parallel build jobs; override with --build-arg BUILD_JOBS=N
ARG BUILD_JOBS=4
ENV MAKEFLAGS=-j${BUILD_JOBS}

ENV BUILD_DIR=/root/ffmpeg-build
RUN mkdir -p ${BUILD_DIR} && \
    echo BUILD_DIR: ${BUILD_DIR} && \
    echo DEPS_CONFIGURE_OPTS: ${DEPS_CONFIGURE_OPTS}

# Install freetype once, but re-install after harfbuzz installed
ARG FREETYPE_VERSION=2.14.3
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://download-mirror.savannah.gnu.org/releases/freetype/freetype-${FREETYPE_VERSION}.tar.gz | tar -zx && \
    cd freetype-${FREETYPE_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure-pre.log && \
    make ${MAKEFLAGS} > make-pre.log 2>&1 && make install 2>&1 | tee -a make-pre.log | tee -a make-pre.log && \
    pkg-config freetype2 --modversion

# Install harfbuzz with freetype support (>= 3.0 uses meson, download from GitHub)
ARG HARFBUZZ_VERSION=14.4.0
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://github.com/harfbuzz/harfbuzz/releases/download/${HARFBUZZ_VERSION}/harfbuzz-${HARFBUZZ_VERSION}.tar.xz | tar -Jx && \
    cd harfbuzz-${HARFBUZZ_VERSION} && \
    meson setup build --prefix=${PREFIX} --default-library=static --buildtype=release -Dfreetype=enabled -Dtests=disabled -Ddocs=disabled 2>&1 | tee -a configure.log && \
    ninja -C build 2>&1 | tee make.log && ninja -C build install 2>&1 | tee -a make.log && \
    pkg-config harfbuzz --modversion

# Re-install freetype with harfbuzz
RUN cd ${BUILD_DIR} && set -o pipefail && cd freetype-${FREETYPE_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure.log && \
    make ${MAKEFLAGS} > make.log 2>&1 && make install 2>&1 | tee -a make.log && make distclean 2>&1 | tee -a make.log && \
    pkg-config freetype2 --modversion

# libfribidi
ARG FRIBIDI_VERSION=1.0.16
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://github.com/fribidi/fribidi/releases/download/v${FRIBIDI_VERSION}/fribidi-${FRIBIDI_VERSION}.tar.xz | tar -Jx && \
    cd fribidi-${FRIBIDI_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config fribidi --modversion

# fontconfig (depends on libexpat)
ARG FONTCONFIG_VERSION=2.18.3
# Without ldconfig, fontconfig fails to build (requires to load libfreetype for cache preloading in `make install`)
RUN ldconfig
# Pull the release tarball from gitlab.freedesktop.org's package registry instead of
# www.freedesktop.org/software/fontconfig/release/. The www host sits behind a "go-away"
# anti-bot CDN that intermittently returns an HTML challenge to non-interactive clients
# (CI runners), which broke the build with `xz: File format not recognized`. The GitLab
# packages endpoint serves the byte-identical upstream tarball without that gate.
RUN cd ${BUILD_DIR} && set -o pipefail && curl -fsSL --retry 3 --retry-delay 5 https://gitlab.freedesktop.org/api/v4/projects/890/packages/generic/fontconfig/${FONTCONFIG_VERSION}/fontconfig-${FONTCONFIG_VERSION}.tar.xz | tar -Jx && \
    cd fontconfig-${FONTCONFIG_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} --disable-docs | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log  && \
    pkg-config fontconfig --modversion

# libass (depends on fontconfig, fridibi)
ARG LIBASS_VERSION=0.17.5
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://github.com/libass/libass/releases/download/${LIBASS_VERSION}/libass-${LIBASS_VERSION}.tar.gz | tar -zx && \
    cd libass-${LIBASS_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} --enable-fontconfig | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config libass --modversion

# x264
# https://trac.ffmpeg.org/wiki/CompilationGuide/Ubuntu
RUN cd ${BUILD_DIR} && set -o pipefail && git clone --branch stable --depth 1 https://code.videolan.org/videolan/x264.git && \
    cd x264 && \
    ./configure ${DEPS_CONFIGURE_OPTS} --enable-pic --enable-static | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config x264 --modversion

# x265
# https://trac.ffmpeg.org/wiki/CompilationGuide/Ubuntu
ARG X265_VERSION=4.2
RUN cd ${BUILD_DIR} && set -o pipefail && git clone --branch ${X265_VERSION} --depth 1 https://bitbucket.org/multicoreware/x265_git && \
    cd x265_git/build/linux && \
    cmake -G "Unix Makefiles" -DCMAKE_INSTALL_PREFIX="${PREFIX}" ../../source 2>&1 | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config x265 --modversion

# ogg
ARG OGG_VERSION=1.3.6
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL http://downloads.xiph.org/releases/ogg/libogg-${OGG_VERSION}.tar.xz | tar -Jx  && \
    cd libogg-${OGG_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config ogg --modversion

# vorbis
ARG VORBIS_VERSION=1.3.7
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL http://downloads.xiph.org/releases/vorbis/libvorbis-${VORBIS_VERSION}.tar.gz | tar -zx && \
    cd libvorbis-${VORBIS_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config vorbis --modversion

# theora
ARG THEORA_VERSION=1.2.0
# The examples/png2theora.c `png_sizeof` macro-removal sed and the config.guess/config.sub
# copy-in for ARM support (needed against theora's 2002-vintage bundled copies) are no longer
# needed as of 1.2.0: png2theora.c no longer uses `png_sizeof`, and theora now bundles a
# 2022 config.guess/config.sub that already recognizes aarch64.
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://ftp.osuosl.org/pub/xiph/releases/theora/libtheora-${THEORA_VERSION}.tar.gz | tar -zx && \
    cd libtheora-${THEORA_VERSION} && \
    ./configure ${DEPS_CONFIGURE_OPTS} --with-ogg=${PREFIX} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config theora --modversion

# lame
ARG LAME_VERSION=4.0
# --disable-decoder: lame 4.0 added an optional on-the-fly mpg123-based decoder (used by the
# `lame` CLI frontend and for more accurate replaygain) and configure hard-errors if libmpg123
# isn't present instead of just skipping it. We only need the libmp3lame encoder that ffmpeg
# links against, so disable it rather than adding a new apt dependency for an unused feature.
#
# --disable-frontend: lame 4.0's frontend/parse.c (the `lame` CLI's ID3v2-tag argument parsing,
# compiled unconditionally, not behind any configure check) calls id3tag_set_comment_ucs2() /
# id3tag_set_fieldvalue_ucs2() - which lame.h no longer declares now that
# DEPRECATED_OR_OBSOLETE_CODE_REMOVED is hardcoded to 1 - and passes a UCS-2 (unsigned short*)
# string into the UTF-8 id3tag_set_textinfo_utf8()/id3tag_set_comment_utf8() calls. Both are
# latent bugs in lame 4.0 itself; they only became fatal because this toolchain's C compiler
# now rejects implicit function declarations and incompatible pointer arguments as hard errors
# by default. We don't need the `lame` CLI binary (only the libmp3lame encoder library that
# ffmpeg links against), so skip building it rather than patching lame's upstream source.
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://sourceforge.net/projects/lame/files/lame/${LAME_VERSION}/lame-${LAME_VERSION}.tar.gz/download | tar -zx && \
    cd lame-${LAME_VERSION} && \
    LAME_ASM_FLAG="" && \
    if [ "$(dpkg --print-architecture)" = "amd64" ]; then LAME_ASM_FLAG="--enable-nasm"; fi && \
    ./configure ${DEPS_CONFIGURE_OPTS} --disable-decoder --disable-frontend ${LAME_ASM_FLAG} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log
    # mp3lame doesn't have pkg-config .pc file

# fdk-aac
ARG FDK_AAC_VERSION=v2.0.3
RUN cd ${BUILD_DIR} && set -o pipefail && git clone --branch ${FDK_AAC_VERSION} --depth 1 https://github.com/mstorsjo/fdk-aac.git && \
    cd fdk-aac && \
    ./autogen.sh | tee -a configure.log && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config fdk-aac --modversion

# opus
ARG OPUS_VERSION=v1.6.1
RUN cd ${BUILD_DIR} && set -o pipefail && git clone --branch ${OPUS_VERSION} --depth 1 https://github.com/xiph/opus.git && \
    cd opus && \
    ./autogen.sh | tee -a configure.log && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config opus --modversion

# vpx
ARG VPX_VERSION=refs/tags/v1.17.0
RUN cd ${BUILD_DIR} && set -o pipefail && git clone https://chromium.googlesource.com/webm/libvpx.git && \
    cd libvpx && git checkout ${VPX_VERSION} && \
    VPX_ASM_FLAG="" && \
    if [ "$(dpkg --print-architecture)" = "amd64" ]; then VPX_ASM_FLAG="--as=yasm"; fi && \
    ./configure ${DEPS_CONFIGURE_OPTS} --disable-examples --disable-unit-tests --enable-vp9-highbitdepth ${VPX_ASM_FLAG} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config vpx --modversion

# AV1 encoder (SvtAv1Enc, library name contains upper-case), requires ffmpeg >= 4.3.3
# 
# Currently we using this across all other AV1 encoders (ref: https://www.osumiakari.jp/articles/20231116-ffmpeg-svtav1/ )
ARG SVTAV1D_VERSION=v4.2.0
RUN cd ${BUILD_DIR} && set -o pipefail && git clone --branch ${SVTAV1D_VERSION} --depth 1 https://gitlab.com/AOMediaCodec/SVT-AV1.git && \
    cd SVT-AV1/Build && \
    cmake -G "Unix Makefiles" -DCMAKE_INSTALL_PREFIX="${PREFIX}" -DCMAKE_BUILD_TYPE=Release -DBUILD_DEC=OFF .. 2>&1 | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config SvtAv1Enc --modversion

# AV1 decoder (dav1d)
ARG DAV1D_VERSION=1.5.4
RUN cd ${BUILD_DIR} && set -o pipefail && git clone --branch ${DAV1D_VERSION} --depth 1 https://code.videolan.org/videolan/dav1d.git && \
    mkdir dav1d/build && cd dav1d/build && \
    meson setup -Denable_tools=false -Denable_tests=false --default-library=static .. --prefix "${PREFIX}" | tee -a configure.log && \
    ninja 2>&1 | tee -a make.log && ninja install 2>&1 | tee -a make.log && \
    pkg-config dav1d --modversion

# webp (library name contains "lib" prefix)
ARG WEBP_VERSION=v1.6.0
RUN cd ${BUILD_DIR} && set -o pipefail && git clone --branch ${WEBP_VERSION} --depth 1 https://chromium.googlesource.com/webm/libwebp && \
    cd libwebp && \
    ./autogen.sh | tee -a configure.log && \
    ./configure ${DEPS_CONFIGURE_OPTS} | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config libwebp --modversion

# libheif: provides the heif-enc / heif-dec command-line tools used to
# encode and decode HEIC/HEIF files. ffmpeg in this image does NOT link
# against libheif (9.0.1 doesn't expose --enable-libheif and reading HEIC
# via the mov demuxer only surfaces the embedded preview JPEG, not the
# full-resolution tile grid), so anything that needs a full-res HEIC →
# JPEG conversion should call heif-dec directly.
#
# Depends on libde265 (HEVC decode) — already brought in via apt above.
#
# Static build (BUILD_SHARED_LIBS=OFF): heif-enc statically links libheif so
# we sidestep a known shared-build issue where the bundled binary mis-links
# against an undefined sequence-API symbol on this image's toolchain.
ARG LIBHEIF_VERSION=1.23.2
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://github.com/strukturag/libheif/releases/download/v${LIBHEIF_VERSION}/libheif-${LIBHEIF_VERSION}.tar.gz | tar -zx && \
    cd libheif-${LIBHEIF_VERSION} && \
    mkdir build && cd build && \
    cmake -DCMAKE_INSTALL_PREFIX=${PREFIX} -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DWITH_EXAMPLES=ON -DWITH_GDK_PIXBUF=OFF .. 2>&1 | tee -a configure.log && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log && \
    pkg-config libheif --modversion && \
    echo "${PREFIX}/lib" > /etc/ld.so.conf.d/local.conf && ldconfig && \
    heif-enc --version | head -1

# NVIDIA codec API headers work on both x86_64 and aarch64. The CUDA/codec
# driver libraries are loaded at runtime from the host via Container Toolkit;
# building NVENC/NVDEC does not require a GPU or the CUDA toolkit.
# Compile CUDA kernels to PTX with LLVM; no CUDA Toolkit or NPP dependency.
RUN apt-get update && apt-get install -y --no-install-recommends clang && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

# https://github.com/FFmpeg/nv-codec-headers/tree/n13.0.19.0
ARG NV_CODEC_HEADERS_VERSION=n13.0.19.0
RUN cd ${BUILD_DIR} && \
    git clone --branch ${NV_CODEC_HEADERS_VERSION} --depth 1 https://github.com/FFmpeg/nv-codec-headers.git && \
    make -C nv-codec-headers PREFIX=${PREFIX} install && \
    pkg-config ffnvcodec --modversion

# ffmpeg, libav
# http://ffmpeg.org/download.html
ARG FFMPEG_VERSION=9.0.1
# Make installed libraries visible before building ffmpeg/libav
RUN ldconfig
# pthread is required by libx265 : https://stackoverflow.com/a/62187983/914786
RUN cd ${BUILD_DIR} && set -o pipefail && curl -sL https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.bz2 | tar -jx && \
    cd ffmpeg* && \
    ./configure --prefix=${PREFIX} \
      --pkg-config-flags="--static" \
      --enable-shared --disable-static \
      --extra-cflags="-O3" \
      --extra-libs="-lpthread -lm" \
      --disable-debug --disable-doc --disable-ffplay \
      --enable-gpl --enable-nonfree --enable-version3 \
      --enable-pthreads \
      --enable-autodetect --enable-swresample --enable-swscale --enable-filters \
      --enable-openssl \
      --enable-ffnvcodec --enable-cuda --enable-cuda-llvm --enable-nvenc --enable-nvdec --enable-cuvid \
      --enable-libwebp \
      --enable-libfreetype --enable-libharfbuzz --enable-libfontconfig --enable-libfribidi --enable-libass --enable-libx264 --enable-libx265  --enable-libvorbis --enable-libtheora --enable-libmp3lame --enable-libfdk-aac --enable-libopus --enable-libvpx --enable-libsvtav1 --enable-libdav1d \
      | tee -a configure.log \
    && \
    make ${MAKEFLAGS} 2>&1 | tee -a make.log && make install 2>&1 | tee -a make.log
RUN ldconfig   # Make ffmpeg libraries visible
RUN ffmpeg -codecs

# Installed as an opt-in command; the image's default command stays unchanged.
COPY --from=agent-build /src/agent/target/release/ffmpeg-agent /usr/local/bin/ffmpeg-agent
COPY agent/LICENSE-MIT agent/LICENSE-APACHE /usr/local/share/licenses/ffmpeg-agent/

# Back to the default
SHELL ["/bin/sh", "-c"]
